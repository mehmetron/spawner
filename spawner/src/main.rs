use crate::{kubernetes::delete_pod, logging::init_logging};
use axum::body::HttpBody;
use clap::Parser;
use dashmap::DashMap;
use hyper::Uri;
use name_generator::NameGenerator;
use serde::Deserialize;
use server::serve;
use std::{
    collections::HashSet,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

mod hashutil;
mod kubernetes;
mod logging;
mod name_generator;
mod server;

#[allow(unused)]
#[derive(Deserialize, Debug)]
pub struct ConnectionState {
    active_connections: u32,
    seconds_inactive: u32,
    listening: bool,
}

#[derive(Parser, Clone)]
pub struct SpawnerParams {
    /// The name of the application image to deploy.
    #[clap(long)]
    application_image: String,

    /// The container port on which the application runs.
    #[clap(long, default_value = "8080")]
    application_port: u16,

    /// The name of the image to deploy as a monitoring sidecar.
    #[clap(long)]
    sidecar_image: Option<String>,

    /// The container port on which the sidecar runs. Only used if
    /// sidecar_image is set.
    #[clap(long, default_value = "7070")]
    sidecar_port: u16,

    /// The prefix used for public-facing URLs. To construct a full URL, it is
    /// appended with the pod name, unless nginx_internal_path is provided, in
    /// which case it is appended with the key.
    #[clap(long)]
    base_url: String,

    /// Prefix used as the prefix for the X-Accel-Redirect header for the
    /// /nginx_redirect endpoint. If set, the key (rather than the name)
    /// is used when constructing URLs.
    #[clap(long)]
    nginx_internal_path: Option<String>,

    /// The scheme used for key generation when nodes are initialized without
    /// a key. Defaults to UUID, other options look like "short:alphanum:5"
    /// (see documentation).
    #[clap(long)]
    name_generator: Option<String>,

    /// The namespace within which pods will be spawned.
    #[clap(long, default_value = "spawner")]
    namespace: String,

    /// How frequently (in seconds) to clean up idle containers.
    #[clap(long, default_value = "30")]
    cleanup_frequency_seconds: u16,
}

#[derive(Clone)]
pub struct SpawnerState {
    application_image: String,
    sidecar_image: Option<String>,
    base_url: String,

    application_port: u16,
    sidecar_port: u16,

    name_generator: Arc<Mutex<NameGenerator>>,
    key_map: Arc<DashMap<String, String>>,
    nginx_internal_path: Option<String>,
    namespace: String,
    cleanup_frequency_seconds: u16,
}

impl SpawnerState {
    pub fn url_for(&self, key: &str, name: &str) -> String {
        if self.nginx_internal_path.is_some() {
            format!("{}/{}/", self.base_url, key)
        } else {
            format!("{}/{}/", self.base_url, name)
        }
    }

    async fn get_pod_state(&self, pod_name: &str) -> anyhow::Result<ConnectionState> {
        // TODO: use monitor port if provided.
        let status_url = Uri::from_str(&format!(
            "http://spawner-{}.{}.svc.cluster.local:{}/status",
            pod_name, self.namespace, self.application_port
        ))
        .expect("Should always be able to construct URL.");
        tracing::info!(%status_url, "Asking container for status.");

        let client = hyper::Client::new();
        let result = client.get(status_url).await?;
        let body = result
            .into_body()
            .data()
            .await
            .ok_or(anyhow::anyhow!("Empty body when ConnectionState expected"))??;

        let connection_state: ConnectionState = serde_json::from_slice(&body)?;
        tracing::info!(?connection_state, "Got connection state.");

        Ok(connection_state)
    }

    pub async fn cleanup_containers(&self) {
        tracing::info!("Cleaning up unused containers.");

        let mut keys_to_remove: HashSet<String> = HashSet::new();

        for container in self.key_map.iter() {
            tracing::info!(key=%container.key(), value=%container.value(), "Checking container.");
            let pod_name = container.value();

            match self.get_pod_state(pod_name).await {
                Ok(connection_state) => {
                    // TODO: don't hard-code duration
                    if connection_state.seconds_inactive > 30 {
                        tracing::info!("Shutting down.");
                        delete_pod(pod_name, &self).await.unwrap();

                        keys_to_remove.insert(container.key().clone());
                    }
                }
                Err(error) => {
                    // TODO: should clean up when a container errors a number of times in a row.
                    tracing::error!(?error, "Encountered error in health check; skipping.")
                }
            }
        }

        for key_to_remove in keys_to_remove {
            tracing::info!(%key_to_remove, "Removing from key map.");
            self.key_map.remove(&key_to_remove);
        }

        tracing::info!("Done cleanup.");
    }
}

#[tokio::main]
async fn main() -> Result<(), kube::Error> {
    let settings = SpawnerParams::parse();

    let name_generator = settings
        .name_generator
        .map(|ng| NameGenerator::from_str(&ng).expect("Could not parse name generator."))
        .unwrap_or_default();

    let state = SpawnerState {
        application_image: settings.application_image,
        application_port: settings.application_port,
        sidecar_image: settings.sidecar_image,
        sidecar_port: settings.sidecar_port,
        base_url: settings.base_url,
        name_generator: Arc::new(Mutex::new(name_generator)),
        key_map: Arc::new(DashMap::default()),
        nginx_internal_path: settings.nginx_internal_path,
        namespace: settings.namespace,
        cleanup_frequency_seconds: settings.cleanup_frequency_seconds,
    };

    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(state.cleanup_frequency_seconds as _)).await;
                state.cleanup_containers().await;
            }
        });
    }

    init_logging();

    serve(state).await
}