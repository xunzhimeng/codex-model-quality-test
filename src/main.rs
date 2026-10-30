mod accounts;
mod app;
mod key_scope;
mod monitor;
mod probe;
mod proxy_transport;
mod questions;
mod store;

use gateway_plugin_sdk::{
    CallBudgets, Stage,
    call::management::{
        ManagementPage, ManagementRegistration, ManagementResource, ManagementRoute,
    },
    client::{Empty, PluginBuilder, PluginSession, SessionConfig, TypedReply, methods},
};
use std::sync::Arc;

fn call_budgets() -> CallBudgets {
    // 后台探针最多180秒，额外30秒供准入、状态回写与停用确认使用。
    [(Stage::Management, 120_000), (Stage::Maintenance, 210_000)]
        .into_iter()
        .collect()
}

fn registration() -> ManagementRegistration {
    ManagementRegistration {
        routes: [
            ("GET", "catalog"),
            ("POST", "models"),
            ("GET", "history"),
            ("POST", "history/delete"),
            ("POST", "questions"),
            ("POST", "run"),
            ("GET", "monitor"),
            ("POST", "monitor"),
            ("POST", "monitor/control"),
            ("POST", "monitor/delete"),
        ]
        .into_iter()
        .map(|(method, path)| ManagementRoute {
            method: method.into(),
            path: path.into(),
            request_content_types: if method == "POST" {
                vec!["application/json".into()]
            } else {
                vec![]
            },
            response_content_types: vec!["application/json".into()],
        })
        .collect(),
        resources: [
            "web/index.html",
            "web/app.js",
            "web/style.css",
            "web/icon.svg",
        ]
        .into_iter()
        .map(|path| ManagementResource {
            path: path.into(),
            public: false,
        })
        .collect(),
        pages: vec![ManagementPage {
            id: "quality".into(),
            title: "模型质量测试".into(),
            description: Some("多题测试与OAuth门票探针".into()),
            entry: "web/index.html".into(),
            icon: None,
        }],
        callbacks: vec![],
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = Arc::new(app::App::default());
    let maintenance = app.clone();
    let plugin = PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .on(methods::RECONCILE, move |call| {
            let app = maintenance.clone();
            async move {
                app.monitor.tick(&app, &call.host).await.map_err(|error| {
                    gateway_plugin_sdk::PluginFault::new(
                        gateway_plugin_sdk::ErrorCode::Fault,
                        error,
                    )
                })?;
                Ok(TypedReply::new(Empty {}))
            }
        })?
        .management(registration(), move |call| {
            let app = app.clone();
            async move {
                match app.handle(call).await {
                    Ok(reply) => Ok(reply),
                    Err(error) => app::json_response(400, &serde_json::json!({"error":error}))
                        .map_err(|_| {
                            gateway_plugin_sdk::PluginFault::new(
                                gateway_plugin_sdk::ErrorCode::Fault,
                                "响应编码失败",
                            )
                        }),
                }
            }
        })?
        .build()?;
    let mut session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    session.set_call_budgets(call_budgets())?;
    session.run(plugin).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_minimum_host_version_without_upper_bound() {
        let source: serde_json::Value =
            serde_json::from_slice(include_bytes!("../plugin.json")).unwrap();
        assert_eq!(source["manifestVersion"], 2);
        assert!(source.get("permissions").is_none());
        assert_eq!(source["engines"]["codex-proxy-rs"], ">=3.33.0");
        assert_eq!(source["version"], env!("CARGO_PKG_VERSION"));
        let manifest =
            gateway_plugin_sdk::Manifest::from_author_slice(include_bytes!("../plugin.json"))
                .unwrap();
        assert_eq!(
            manifest.contributes.keys().copied().collect::<Vec<_>>(),
            vec![
                gateway_plugin_sdk::Capability::Management,
                gateway_plugin_sdk::Capability::Maintenance
            ]
        );
        assert_eq!(source["contributes"]["management"]["version"], 2);
        assert_eq!(manifest.state[0].schema_version, 1);
        assert_eq!(manifest.state[0].namespace, "quality");
        assert_eq!(gateway_plugin_sdk::PROTOCOL_VERSION, 7);
    }

    #[test]
    fn declared_budgets_cover_background_probe_and_state_writeback() {
        let declarations = call_budgets();
        let resolved = gateway_plugin_sdk::ResolvedCallBudgets::resolve(&declarations).unwrap();
        assert_eq!(resolved.budget(Stage::Management).as_millis(), 120_000);
        assert_eq!(resolved.budget(Stage::Maintenance).as_millis(), 210_000);
        assert_eq!(
            resolved.source(Stage::Management),
            gateway_plugin_sdk::CallBudgetSource::Plugin
        );
        assert_eq!(resolved.budget(Stage::Registration).as_millis(), 5_000);
        assert_eq!(
            resolved.source(Stage::Registration),
            gateway_plugin_sdk::CallBudgetSource::Default
        );
    }
    #[test]
    fn management_paths_are_relative() {
        // 管理路径挂在实例命名空间下，前导斜杠会被宿主作为空路径段拒绝。
        let registration = registration();
        let mut endpoints = std::collections::BTreeSet::new();
        for route in &registration.routes {
            assert!(!route.path.starts_with('/'));
            assert!(route.path.split('/').all(|part| !part.is_empty()));
            assert!(endpoints.insert((&route.method, &route.path)));
        }
        for resource in &registration.resources {
            assert!(!resource.path.starts_with('/'));
        }
        for page in &registration.pages {
            assert!(
                registration
                    .resources
                    .iter()
                    .any(|resource| resource.path == page.entry)
            );
        }
    }

    #[test]
    fn manifest_and_registration_agree() {
        let builder = PluginBuilder::from_json(include_bytes!("../plugin.json")).unwrap();
        assert!(
            builder
                .on(methods::RECONCILE, |_| async {
                    Ok(TypedReply::new(Empty {}))
                })
                .unwrap()
                .management(registration(), |_| async {
                    app::json_response(200, &serde_json::json!({})).map_err(|_| {
                        gateway_plugin_sdk::PluginFault::new(
                            gateway_plugin_sdk::ErrorCode::Fault,
                            "编码失败",
                        )
                    })
                })
                .unwrap()
                .build()
                .is_ok()
        );
    }
}
