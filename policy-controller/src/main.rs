#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::Parser;
use futures::{future, prelude::*};
use linkerd_policy_controller::k8s::DefaultPolicy;
use linkerd_policy_controller::{admin, admission};
use linkerd_policy_controller_core::IpNet;
use std::net::SocketAddr;
use tokio::{sync::watch, time};
use tracing::{debug, info, info_span, instrument, Instrument};

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[derive(Debug, Parser)]
#[clap(name = "policy", about = "A policy resource prototype")]
struct Args {
    #[clap(
        parse(try_from_str),
        long,
        default_value = "linkerd=info,warn",
        env = "LINKERD_POLICY_CONTROLLER_LOG"
    )]
    log_level: kubert::log::EnvFilter,

    #[clap(long, default_value = "plain")]
    log_format: kubert::log::LogFormat,

    #[clap(flatten)]
    client: kubert::ClientArgs,

    #[clap(flatten)]
    webhook: kubert::webhook::WebhookArgs,

    #[clap(long, default_value = "0.0.0.0:8080")]
    admin_addr: SocketAddr,

    #[clap(long, default_value = "0.0.0.0:8090")]
    grpc_addr: SocketAddr,

    /// Network CIDRs of pod IPs.
    ///
    /// The default includes all private networks.
    #[clap(
        long,
        default_value = "10.0.0.0/8,100.64.0.0/10,172.16.0.0/12,192.168.0.0/16"
    )]
    cluster_networks: IpNets,

    #[clap(long, default_value = "cluster.local")]
    identity_domain: String,

    #[clap(long, default_value = "all-unauthenticated")]
    default_policy: DefaultPolicy,

    #[clap(long, default_value = "linkerd")]
    control_plane_namespace: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let Args {
        client,
        admin_addr,
        grpc_addr,
        webhook,
        identity_domain,
        cluster_networks: IpNets(cluster_networks),
        default_policy,
        log_level,
        log_format,
        control_plane_namespace,
    } = Args::parse();

    log_format.try_init(log_level)?;

    let (drain_tx, drain_rx) = drain::channel();

    // Load a Kubernetes client from the environment (check for in-cluster configuration first).
    let client = client
        .try_client()
        .await
        .context("failed to initialize kubernetes client")?;

    // Spawn an admin server, failing readiness checks until the index is updated.
    let (ready_tx, ready_rx) = watch::channel(false);
    tokio::spawn(admin::serve(admin_addr, ready_rx));

    // Index cluster resources, returning a handle that supports lookups for the gRPC server.
    let handle = {
        const DETECT_TIMEOUT: time::Duration = time::Duration::from_secs(10);
        let cluster = linkerd_policy_controller::k8s::ClusterInfo {
            networks: cluster_networks.clone(),
            identity_domain,
            control_plane_ns: control_plane_namespace,
        };
        let (handle, index) =
            linkerd_policy_controller::k8s::Index::new(cluster, default_policy, DETECT_TIMEOUT);

        tokio::spawn(index.run(client.clone(), ready_tx));
        handle
    };

    // Run the gRPC server, serving results by looking up against the index handle.
    tokio::spawn(grpc(grpc_addr, cluster_networks, handle, drain_rx.clone()));

    let (listen_addr, serve) = webhook.serve(admission::Service { client }).await?;
    tokio::spawn(serve.instrument(info_span!("webhook")));
    info!(addr = %listen_addr, "Admission controller server listening");

    // Block the main thread on the shutdown signal. Once it fires, wait for the background tasks to
    // complete before exiting.
    shutdown(drain_tx).await;

    Ok(())
}

#[derive(Debug)]
struct IpNets(Vec<IpNet>);

impl std::str::FromStr for IpNets {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        s.split(',')
            .map(|n| n.parse().map_err(Into::into))
            .collect::<Result<Vec<IpNet>>>()
            .map(Self)
    }
}

#[instrument(skip(handle, drain))]
async fn grpc(
    addr: SocketAddr,
    cluster_networks: Vec<IpNet>,
    handle: linkerd_policy_controller_k8s_index::Reader,
    drain: drain::Watch,
) -> Result<()> {
    let server =
        linkerd_policy_controller_grpc::Server::new(handle, cluster_networks, drain.clone());
    let (close_tx, close_rx) = tokio::sync::oneshot::channel();
    tokio::pin! {
        let srv = server.serve(addr, close_rx.map(|_| {}));
    }
    info!(%addr, "gRPC server listening");
    tokio::select! {
        res = (&mut srv) => res?,
        handle = drain.signaled() => {
            let _ = close_tx.send(());
            handle.release_after(srv).await?
        }
    }
    Ok(())
}

async fn shutdown(drain: drain::Signal) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            debug!("Received ctrl-c");
        },
        _ = sigterm() => {
            debug!("Received SIGTERM");
        }
    }
    info!("Shutting down");
    drain.drain().await;
}

async fn sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => term.recv().await,
        _ => future::pending().await,
    };
}
