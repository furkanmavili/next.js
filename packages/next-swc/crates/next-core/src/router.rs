use anyhow::{bail, Context, Result};
use futures::StreamExt;
use indexmap::indexmap;
use serde::Deserialize;
use serde_json::json;
use turbo_tasks::{
    primitives::{JsonValueVc, StringsVc},
    CompletionVc, CompletionsVc, Value,
};
use turbo_tasks_bytes::{Bytes, Stream};
use turbo_tasks_fs::{json::parse_json_with_source_context, to_sys_path, File, FileSystemPathVc};
use turbopack::{evaluate_context::node_evaluate_asset_context, transition::TransitionsByNameVc};
use turbopack_core::{
    asset::AssetVc,
    changed::any_content_changed,
    chunk::dev::DevChunkingContextVc,
    context::{AssetContext, AssetContextVc},
    environment::{EnvironmentIntention::Middleware, ServerAddrVc},
    ident::AssetIdentVc,
    issue::IssueVc,
    reference_type::{EcmaScriptModulesReferenceSubType, ReferenceType},
    resolve::{find_context_file, FindContextFileResult},
    source_asset::SourceAssetVc,
    virtual_asset::VirtualAssetVc,
};
use turbopack_ecmascript::{
    EcmascriptInputTransform, EcmascriptInputTransformsVc, EcmascriptModuleAssetType,
    EcmascriptModuleAssetVc, InnerAssetsVc, OptionEcmascriptModuleAssetVc,
};
use turbopack_node::{
    evaluate::{evaluate, JavaScriptEvaluation},
    execution_context::{ExecutionContext, ExecutionContextVc},
    StructuredError,
};

use crate::{
    embed_js::{next_asset, next_js_file},
    next_config::NextConfigVc,
    next_edge::{
        context::{get_edge_compile_time_info, get_edge_resolve_options_context},
        transition::NextEdgeTransition,
    },
    next_import_map::get_next_build_import_map,
    next_server::context::{get_server_module_options_context, ServerContextType},
    util::{parse_config_from_source, NextSourceConfigVc},
};

#[turbo_tasks::function]
fn next_configs() -> StringsVc {
    StringsVc::cell(
        ["next.config.mjs", "next.config.js"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
    )
}

#[turbo_tasks::function]
async fn middleware_files(page_extensions: StringsVc) -> Result<StringsVc> {
    let extensions = page_extensions.await?;
    let files = ["middleware.", "src/middleware."]
        .into_iter()
        .flat_map(|f| {
            extensions
                .iter()
                .map(move |ext| String::from(f) + ext.as_str())
        })
        .collect();
    Ok(StringsVc::cell(files))
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct RouterRequest {
    pub method: String,
    pub pathname: String,
    pub raw_query: String,
    pub raw_headers: Vec<(String, String)>,
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct RewriteResponse {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct MiddlewareHeadersResponse {
    pub status_code: u16,
    pub headers: Vec<(String, String)>,
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
pub struct MiddlewareBodyResponse(Bytes);

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum RouterIncomingMessage {
    Rewrite { data: RewriteResponse },
    MiddlewareHeaders { data: MiddlewareHeadersResponse },
    MiddlewareBody { data: Vec<u8> },
    None,
    Error(StructuredError),
}

#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Default)]
pub struct MiddlewareResponse {
    pub status_code: u16,
    pub headers: Vec<(String, String)>,
    #[turbo_tasks(trace_ignore)]
    pub body: Stream<Result<Bytes, String>>,
}

#[derive(Debug)]
#[turbo_tasks::value]
pub enum RouterResult {
    Rewrite(RewriteResponse),
    Middleware(MiddlewareResponse),
    None,
    Error,
}

#[turbo_tasks::function]
async fn get_config(
    context: AssetContextVc,
    project_path: FileSystemPathVc,
    configs: StringsVc,
) -> Result<OptionEcmascriptModuleAssetVc> {
    let find_config_result = find_context_file(project_path, configs);
    let config_asset = match &*find_config_result.await? {
        FindContextFileResult::Found(config_path, _) => Some(as_es_module_asset(
            SourceAssetVc::new(*config_path).as_asset(),
            context,
        )),
        FindContextFileResult::NotFound(_) => None,
    };
    Ok(OptionEcmascriptModuleAssetVc::cell(config_asset))
}

fn as_es_module_asset(asset: AssetVc, context: AssetContextVc) -> EcmascriptModuleAssetVc {
    EcmascriptModuleAssetVc::new(
        asset,
        context,
        Value::new(EcmascriptModuleAssetType::Typescript),
        EcmascriptInputTransformsVc::cell(vec![EcmascriptInputTransform::TypeScript {
            use_define_for_class_fields: false,
        }]),
        context.compile_time_info(),
    )
}

#[turbo_tasks::function]
async fn next_config_changed(
    context: AssetContextVc,
    project_path: FileSystemPathVc,
) -> Result<CompletionVc> {
    let next_config = get_config(context, project_path, next_configs()).await?;
    Ok(if let Some(c) = *next_config {
        any_content_changed(c.into())
    } else {
        CompletionVc::immutable()
    })
}

#[turbo_tasks::function]
async fn config_assets(
    context: AssetContextVc,
    project_path: FileSystemPathVc,
    page_extensions: StringsVc,
) -> Result<InnerAssetsVc> {
    let middleware_config =
        get_config(context, project_path, middleware_files(page_extensions)).await?;

    // The router.ts file expects a manifest of chunks for the middleware. If there
    // is no middleware file, then we need to generate a default empty manifest
    // and we cannot process it with the next-edge transition because it
    // requires a real file for some reason.
    let (manifest, config) = match &*middleware_config {
        Some(c) => {
            let manifest = context.with_transition("next-edge").process(
                c.as_asset(),
                Value::new(ReferenceType::EcmaScriptModules(
                    EcmaScriptModulesReferenceSubType::Undefined,
                )),
            );
            let config = parse_config_from_source(c.as_asset());
            (manifest, config)
        }
        None => {
            let manifest = as_es_module_asset(
                VirtualAssetVc::new(
                    project_path.join("middleware.js"),
                    File::from("export default [];").into(),
                )
                .as_asset(),
                context,
            )
            .as_asset();
            let config = NextSourceConfigVc::default();
            (manifest, config)
        }
    };

    let config_asset = as_es_module_asset(
        VirtualAssetVc::new(
            project_path.join("middleware_config.js"),
            File::from(format!(
                "export default {};",
                json!({ "matcher": &config.await?.matcher })
            ))
            .into(),
        )
        .as_asset(),
        context,
    )
    .as_asset();

    Ok(InnerAssetsVc::cell(indexmap! {
        "MIDDLEWARE_CHUNK_GROUP".to_string() => manifest,
        "MIDDLEWARE_CONFIG".to_string() => config_asset,
    }))
}

#[turbo_tasks::function]
fn route_executor(context: AssetContextVc, configs: InnerAssetsVc) -> AssetVc {
    EcmascriptModuleAssetVc::new_with_inner_assets(
        next_asset("entry/router.ts"),
        context,
        Value::new(EcmascriptModuleAssetType::Typescript),
        EcmascriptInputTransformsVc::cell(vec![EcmascriptInputTransform::TypeScript {
            use_define_for_class_fields: false,
        }]),
        context.compile_time_info(),
        configs,
    )
    .into()
}

#[turbo_tasks::function]
fn edge_transition_map(
    server_addr: ServerAddrVc,
    project_path: FileSystemPathVc,
    output_path: FileSystemPathVc,
    next_config: NextConfigVc,
    execution_context: ExecutionContextVc,
) -> TransitionsByNameVc {
    let edge_compile_time_info = get_edge_compile_time_info(server_addr, Value::new(Middleware));

    let edge_chunking_context = DevChunkingContextVc::builder(
        project_path,
        output_path.join("edge"),
        output_path.join("edge/chunks"),
        output_path.join("edge/assets"),
        edge_compile_time_info.environment(),
    )
    .build();

    let edge_resolve_options_context = get_edge_resolve_options_context(
        project_path,
        Value::new(ServerContextType::Middleware),
        next_config,
        execution_context,
    );

    let server_module_options_context = get_server_module_options_context(
        project_path,
        execution_context,
        Value::new(ServerContextType::Middleware),
        next_config,
    );

    let next_edge_transition = NextEdgeTransition {
        edge_compile_time_info,
        edge_chunking_context,
        edge_module_options_context: Some(server_module_options_context),
        edge_resolve_options_context,
        output_path: output_path.root(),
        base_path: project_path,
        bootstrap_file: next_js_file("entry/edge-bootstrap.ts"),
        entry_name: "middleware".to_string(),
    }
    .cell()
    .into();

    TransitionsByNameVc::cell(
        [("next-edge".to_string(), next_edge_transition)]
            .into_iter()
            .collect(),
    )
}

#[turbo_tasks::function]
pub async fn route(
    execution_context: ExecutionContextVc,
    request: RouterRequestVc,
    next_config: NextConfigVc,
    server_addr: ServerAddrVc,
    routes_changed: CompletionVc,
) -> Result<RouterResultVc> {
    let RouterRequest {
        ref method,
        ref pathname,
        ..
    } = *request.await?;
    IssueVc::attach_description(
        format!("Next.js Routing for {} {}", method, pathname),
        route_internal(
            execution_context,
            request,
            next_config,
            server_addr,
            routes_changed,
        ),
    )
    .await
}

#[turbo_tasks::function]
async fn route_internal(
    execution_context: ExecutionContextVc,
    request: RouterRequestVc,
    next_config: NextConfigVc,
    server_addr: ServerAddrVc,
    routes_changed: CompletionVc,
) -> Result<RouterResultVc> {
    let ExecutionContext {
        project_path,
        intermediate_output_path,
        env,
    } = *execution_context.await?;
    let intermediate_output_path = intermediate_output_path.join("router");

    let context = node_evaluate_asset_context(
        project_path,
        Some(get_next_build_import_map()),
        Some(edge_transition_map(
            server_addr,
            project_path,
            intermediate_output_path,
            next_config,
            execution_context,
        )),
    );

    let configs = config_assets(context, project_path, next_config.page_extensions());
    let router_asset = route_executor(context, configs);

    // This invalidates the router when the next config changes
    let next_config_changed = next_config_changed(context, project_path);

    let request = serde_json::value::to_value(&*request.await?)?;
    let Some(dir) = to_sys_path(project_path).await? else {
        bail!("Next.js requires a disk path to check for valid routes");
    };
    let result = evaluate(
        project_path,
        router_asset,
        project_path,
        env,
        AssetIdentVc::from_path(project_path),
        context,
        intermediate_output_path,
        None,
        vec![
            JsonValueVc::cell(request),
            JsonValueVc::cell(dir.to_string_lossy().into()),
        ],
        CompletionsVc::all(vec![next_config_changed, routes_changed]),
        /* debug */ false,
    )
    .await?;

    match &*result {
        JavaScriptEvaluation::Single(Ok(val)) => {
            // If we received a single message from the router, then it must be a
            // rewrite/none/error. Middleware always comes in 2+ chunk streams.
            let result: RouterIncomingMessage = parse_json_with_source_context(val.to_str()?)?;
            Ok(match result {
                RouterIncomingMessage::Rewrite { data } => RouterResult::Rewrite(data),
                RouterIncomingMessage::None => RouterResult::None,
                _ => RouterResult::Error,
            }
            .cell())
        }
        JavaScriptEvaluation::Stream(stream) => {
            let mut read = stream.read();

            let middleware = match read.next().await {
                Some(Ok(first)) => {
                    // The first chunk is the Middleware's headers/status. Everything after that is
                    // body contents.
                    let headers: RouterIncomingMessage =
                        parse_json_with_source_context(first.to_str()?)?;

                    // The double encoding here is annoying. It'd be a lot nicer if we could embed
                    // a buffer directly into the IPC message without having to wrap it in an
                    // object.
                    let body = read.map(|data| {
                        let chunk = match data {
                            Ok(c) => c,
                            Err(e) => return Err(e.message),
                        };
                        let chunk: RouterIncomingMessage = match chunk
                            .to_str()
                            .context("error decoding string")
                            .and_then(parse_json_with_source_context)
                        {
                            Ok(c) => c,
                            Err(e) => return Err(e.to_string()),
                        };
                        match chunk {
                            RouterIncomingMessage::MiddlewareBody { data } => Ok(Bytes::from(data)),
                            m => Err(format!("unexpected message type: {:#?}", m)),
                        }
                    });

                    match headers {
                        RouterIncomingMessage::MiddlewareHeaders { data } => MiddlewareResponse {
                            status_code: data.status_code,
                            headers: data.headers,
                            body: Stream::from_stream(body),
                        },
                        _ => return Ok(RouterResult::Error.cell()),
                    }
                }
                _ => return Ok(RouterResult::Error.cell()),
            };
            Ok(RouterResult::Middleware(middleware).cell())
        }
        _ => Ok(RouterResult::Error.cell()),
    }
}
