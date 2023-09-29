//! PyPI source.
//!
//! Pypi is a source storage which scans PyPI. The snapshot is generated by first
//! scanning the package index, then scanning index of every package. This only takes
//! about 5 minutes on SJTUG server, where we fetch data from TUNA mirrors.
//! A PyPI link may contain checksum in its URL, and when taking snapshot, this source
//! will remove checksums from URL.
//!
//! Pypi supports path snapshot, and TransferURL source object.

use std::env;

use async_trait::async_trait;
use futures_util::{stream, StreamExt, TryStreamExt};
use google_bigquery2::api::QueryRequest;
use google_bigquery2::hyper::client::HttpConnector;
use google_bigquery2::hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use google_bigquery2::oauth2::authenticator::ApplicationDefaultCredentialsTypes;
use google_bigquery2::oauth2::{
    ApplicationDefaultCredentialsAuthenticator, ApplicationDefaultCredentialsFlowOpts,
};
use google_bigquery2::{hyper, Bigquery};
use hyper_proxy::{Intercept, Proxy, ProxyConnector};
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use slog::{info, warn, Logger};
use structopt::StructOpt;

use crate::common::{Mission, SnapshotConfig, SnapshotPath, TransferURL};
use crate::error::{Error, Result};
use crate::python_version::Version;
use crate::traits::{SnapshotStorage, SourceStorage};
use crate::utils::bar;

const BQ_QUERY: &str = r#"
    SELECT file.project, COUNT(*) AS num_downloads
    FROM `bigquery-public-data.pypi.file_downloads`
    WHERE
      details.installer.name = 'pip'
      AND
      DATE(timestamp)
        BETWEEN DATE_SUB(CURRENT_DATE(), INTERVAL 1 DAY)
        AND CURRENT_DATE()
    GROUP BY file.project
    ORDER BY num_downloads DESC
    LIMIT 1000;
    "#;

#[derive(Debug, Clone, StructOpt)]
pub struct Pypi {
    /// Base of simple index
    #[structopt(
        long,
        default_value = "https://mirrors.tuna.tsinghua.edu.cn/pypi/web/simple",
        help = "Base of simple index"
    )]
    pub simple_base: String,
    /// Base of package base
    #[structopt(
        long,
        default_value = "https://mirrors.tuna.tsinghua.edu.cn/pypi/web/packages",
        help = "Base of package index"
    )]
    pub package_base: String,
    /// When set, the source will query bigquery for indexing and only the first 1000 most
    /// downloaded packages will be selected.
    /// Please consider adding `--no-delete` parameter on simple diff transfer to avoid clearing
    /// previous cache.
    #[structopt(long)]
    pub bq_query: bool,
    /// Only keep recent N versions per package.
    /// Please consider adding `--no-delete` parameter on simple diff transfer to avoid clearing
    /// previous cache.
    #[structopt(long)]
    pub keep_recent: Option<usize>,
    /// When debug mode is enabled, only first 1000 packages will be selected.
    /// Please add `--no-delete` parameter on simple diff transfer when enabling
    /// debug mode on a production endpoint.
    #[structopt(long)]
    pub debug: bool,
}

async fn pypi_index(
    logger: &Logger,
    client: &Client,
    simple_base: &str,
    debug: bool,
) -> Result<Vec<String>> {
    info!(logger, "downloading pypi index...");
    let mut index = client
        .get(&format!("{}/", simple_base))
        .send()
        .await?
        .text()
        .await?;

    info!(logger, "parsing index...");
    let matcher = Regex::new(r#"<a.*href=".*?".*>(.*?)</a>"#).unwrap();
    if debug {
        index = index[..1000].to_string();
    }
    Ok(matcher
        .captures_iter(&index)
        .map(|cap| cap[1].to_string())
        .collect())
}

macro_rules! append_proxy_from_env {
    ($proxies:expr, $env_name:expr, $intercept:expr) => {
        if let Ok(proxy) = env::var($env_name) {
            $proxies.push(Proxy::new($intercept, proxy.parse().expect($env_name)));
        }
    };
}

fn collect_proxies() -> Vec<Proxy> {
    let mut proxies = vec![];

    // TODO: priority?
    append_proxy_from_env!(proxies, "http_proxy", Intercept::Http);
    append_proxy_from_env!(proxies, "HTTP_PROXY", Intercept::Http);
    append_proxy_from_env!(proxies, "https_proxy", Intercept::Https);
    append_proxy_from_env!(proxies, "HTTPS_PROXY", Intercept::Https);
    append_proxy_from_env!(proxies, "all_proxy", Intercept::All);
    append_proxy_from_env!(proxies, "ALL_PROXY", Intercept::All);

    proxies
}

fn hyper_client() -> Result<hyper::Client<ProxyConnector<HttpsConnector<HttpConnector>>>> {
    let raw_connector = HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    let mut connector = ProxyConnector::new(raw_connector)?;
    connector.extend_proxies(collect_proxies());
    Ok(hyper::Client::builder().build(connector))
}

async fn bigquery_hub() -> Result<Bigquery<ProxyConnector<HttpsConnector<HttpConnector>>>> {
    let hyper = hyper_client()?;
    let auth = match ApplicationDefaultCredentialsAuthenticator::with_client(
        ApplicationDefaultCredentialsFlowOpts::default(),
        hyper.clone(),
    )
    .await
    {
        ApplicationDefaultCredentialsTypes::ServiceAccount(authenticator) => {
            authenticator.build().await?
        }
        ApplicationDefaultCredentialsTypes::InstanceMetadata(authenticator) => {
            authenticator.build().await?
        }
    };
    Ok(Bigquery::new(hyper, auth))
}

async fn bigquery_index(logger: &Logger) -> Result<Vec<String>> {
    info!(logger, "executing bigquery query...");
    let prj_id = env::var("PROJECT_ID").expect("Environment variable PROJECT_ID");

    let hub = bigquery_hub().await?;

    let (_, resp) = hub
        .jobs()
        .query(
            QueryRequest {
                query: Some(BQ_QUERY.to_string()),
                use_legacy_sql: Some(false),
                ..Default::default()
            },
            &prj_id,
        )
        .doit()
        .await?;

    Ok(resp
        .rows
        .expect("rows")
        .into_iter()
        .map(|row| {
            let row = row.f.expect("columns");
            match row.into_iter().next().expect("project").v {
                Some(Value::String(s)) => s,
                _ => panic!("invalid project name"),
            }
        })
        .collect())
}

fn version_from_filename(filename: &str) -> Option<Version> {
    static RE_VERSION: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
        Regex::new(r"^\w+-([\w.-_+]+).*(.tar.gz|tar.bz2|.zip|.whl|.exe|.egg)$").unwrap()
    });
    RE_VERSION
        .captures(filename)
        .and_then(|cap| cap.get(1))
        .and_then(|cap| Version::parse(cap.as_str()).ok())
}

fn truncate_to_recent(
    logger: &Logger,
    package: &str,
    entries: Vec<(String, String)>,
    keep_recent: usize,
) -> Vec<(String, String)> {
    let candidates: Option<Vec<_>> = entries
        .iter()
        .map(|(url, name)| {
            if let Some(version) = version_from_filename(name) {
                Some((url, name, version))
            } else {
                warn!(logger, "failed to parse version from filename: {}", name);
                None
            }
        })
        .collect();
    if let Some(mut candidates) = candidates {
        candidates.sort_by_key(|(_, _, version)| version.clone());
        let mut result = vec![];
        let at_most_unstable = keep_recent / 2;
        let mut selected_count = 0;
        let mut selected_unstable_count = 0;
        let mut prev = None;
        for (url, name, version) in candidates.into_iter().rev() {
            if prev.as_ref() == Some(&version) {
                // Another file of this version is already selected. Select this too.
                result.push((url.clone(), name.clone()));
                continue;
            }
            if selected_count >= keep_recent {
                // There's enough versions, stop here.
                break;
            }

            // A new version is encountered.
            if version.is_stable() {
                // We'd like to pick stable versions first.
                result.push((url.clone(), name.clone()));
            } else {
                // If it's not an unstable version, pick it only if we haven't selected enough.
                if selected_unstable_count >= at_most_unstable {
                    continue;
                }
                result.push((url.clone(), name.clone()));
                selected_unstable_count += 1;
            }
            prev = Some(version);
            selected_count += 1;
        }
        result
    } else {
        warn!(logger, "give up keep_recent for package: {}", package);
        entries
    }
}

#[async_trait]
impl SnapshotStorage<SnapshotPath> for Pypi {
    async fn snapshot(
        &mut self,
        mission: Mission,
        config: &SnapshotConfig,
    ) -> Result<Vec<SnapshotPath>> {
        let logger = mission.logger;
        let progress = mission.progress;
        let client = mission.client;

        let projects = if self.bq_query {
            if self.debug {
                warn!(logger, "debug mode is ignored in bigquery mode");
            }
            bigquery_index(&logger).await?
        } else {
            pypi_index(&logger, &client, &self.simple_base, self.debug).await?
        };

        info!(logger, "downloading package index...");
        progress.set_length(projects.len() as u64);
        progress.set_style(bar());

        let matcher = Regex::new(r#"<a.*href="(.*?)".*>(.*?)</a>"#).unwrap();
        let packages: Result<Vec<Vec<(String, String)>>> =
            stream::iter(projects.into_iter().map(|name| {
                let client = client.clone();
                let simple_base = self.simple_base.clone();
                let keep_recent = self.keep_recent;
                let progress = progress.clone();
                let matcher = matcher.clone();
                let logger = logger.clone();

                let func = {
                    let logger = logger.clone();
                    async move {
                        progress.set_message(&name);
                        let package = client
                            .get(&format!("{}/{}/", simple_base, name))
                            .send()
                            .await?
                            .text()
                            .await?;
                        let caps: Vec<(String, String)> = matcher
                            .captures_iter(&package)
                            .map(|cap| {
                                let url = format!("{}/{}/{}", simple_base, name, &cap[1]);
                                let parsed = url::Url::parse(&url).unwrap();
                                let cleaned: &str = &parsed[..url::Position::AfterPath];
                                (cleaned.to_string(), cap[2].to_string())
                            })
                            .collect();
                        let caps = if let Some(keep_recent) = keep_recent {
                            truncate_to_recent(&logger, &name, caps, keep_recent)
                        } else {
                            caps
                        };
                        progress.inc(1);
                        Ok::<Vec<(String, String)>, Error>(caps)
                    }
                };
                async move {
                    match func.await {
                        Ok(x) => Ok(x),
                        Err(err) => {
                            warn!(logger, "failed to fetch index {:?}", err);
                            Ok(vec![])
                        }
                    }
                }
            }))
            .buffer_unordered(config.concurrent_resolve)
            .try_collect()
            .await;

        let package_base = if self.package_base.ends_with('/') {
            self.package_base.clone()
        } else {
            format!("{}/", self.package_base)
        };

        let snapshot = packages?
            .into_iter()
            .flatten()
            .filter_map(|(url, _)| {
                if url.starts_with(&package_base) {
                    Some(url[package_base.len()..].to_string())
                } else {
                    warn!(logger, "PyPI package isn't stored on base: {:?}", url);
                    None
                }
            })
            .collect();

        progress.finish_with_message("done");

        Ok(crate::utils::snapshot_string_to_path(snapshot))
    }

    fn info(&self) -> String {
        format!("pypi, {:?}", self)
    }
}

#[async_trait]
impl SourceStorage<SnapshotPath, TransferURL> for Pypi {
    async fn get_object(&self, snapshot: &SnapshotPath, _mission: &Mission) -> Result<TransferURL> {
        Ok(TransferURL(format!("{}/{}", self.package_base, snapshot.0)))
    }
}
