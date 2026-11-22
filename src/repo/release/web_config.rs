//! NGINX configuration for serving CPS web roots or a read-through cache.
//!
//! Versioned metadata and targets never change once published, so they are
//! served and cached as immutable. `timestamp.json` and `publisher.json` are
//! the only mutable files: they are revalidated by browsers and never cached
//! by a read-through cache. Everything else is refused.
//!
//! One server can serve several publishers, each under its own path prefix,
//! such as `/canopa/`.

use std::{fmt::Write, path::Path};

use anyhow::{Context, Result, ensure};

use crate::model::digest;

/// What the generated server does.
pub(crate) enum WebConfig<'a> {
    /// Serve deployed web roots or replicas from disk, by path prefix.
    Static { roots: &'a [Mount<&'a Path>] },
    /// Cache upstream CPS web roots on a LAN, by path prefix.
    Cache {
        directory: &'a Path,
        upstreams: &'a [Mount<&'a str>],
        max_size: &'a str,
        ca_bundle: &'a Path,
    },
}

/// A web root or upstream served under a path prefix such as `/canopa/`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Mount<T> {
    /// Starts and ends with `/`.
    pub prefix: String,
    pub source: T,
}

impl Mount<String> {
    /// Parse `[PREFIX=]VALUE`. Without a prefix, the value is served at `/`.
    pub(crate) fn parse(spec: &str) -> Result<Self> {
        let (prefix, source) = match spec.split_once('=') {
            Some((prefix, source)) => (prefix, source),
            None => ("/", spec),
        };
        ensure!(
            prefix.starts_with('/')
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
                && prefix
                    .trim_matches('/')
                    .split('/')
                    .all(|segment| !matches!(segment, "." | ".."))
                && !prefix.contains("//"),
            "use a path prefix such as /canopa, not {prefix:?}"
        );
        let mut prefix = prefix.to_owned();
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        Ok(Self {
            prefix,
            source: source.to_owned(),
        })
    }
}

/// TLS settings of the generated HTTPS server.
pub(crate) struct WebServer<'a> {
    pub host: &'a str,
    pub certificate: &'a Path,
    pub key: &'a Path,
}

const METADATA: &str = r"metadata/[0-9]+\.[a-z0-9-]+\.json$";
const TARGETS: &str = r"targets/[a-f0-9]{64}\.[a-zA-Z0-9._-]+$";
const IMMUTABLE: &str = r#"add_header Cache-Control "public, max-age=604800, immutable";"#;
const REVALIDATE: &str = r#"add_header Cache-Control "no-cache";"#;

/// Paths are written into the configuration verbatim, so they may not hold
/// spaces or NGINX syntax.
fn check_path(path: &Path) -> Result<&str> {
    let text = path.to_str().unwrap_or("");
    ensure!(
        text.starts_with('/')
            && text != "/"
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._/+@-".contains(&byte)),
        "use an absolute path without spaces or NGINX metacharacters, not {}",
        path.display()
    );
    Ok(text)
}

fn check_host(host: &str) -> Result<()> {
    ensure!(
        host.len() <= 253
            && host
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte)),
        "invalid HTTPS host name {host:?}"
    );
    Ok(())
}

/// The upstream base URL as written into `proxy_pass`, rebuilt from checked
/// parts so that nothing else reaches the configuration.
fn check_upstream(upstream: &str) -> Result<String> {
    let invalid = || {
        format!(
            "use a fixed https:// upstream base URL without credentials, query, fragment, \
             or trailing slash, not {upstream:?}"
        )
    };
    let url = url::Url::parse(upstream).with_context(invalid)?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url
                .path()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._~/-".contains(&byte))
            && !url.path().contains("//")
            && !upstream.ends_with('/'),
        invalid()
    );
    let host = match url.host().with_context(invalid)? {
        url::Host::Domain(host) => {
            check_host(host)?;
            host.to_owned()
        }
        url::Host::Ipv4(address) => address.to_string(),
        url::Host::Ipv6(address) => format!("[{address}]"),
    };
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Ok(format!(
        "https://{host}{port}{}",
        url.path().trim_end_matches('/')
    ))
}

fn check_size(size: &str) -> Result<()> {
    let digits = size.len().saturating_sub(1);
    ensure!(
        digits > 0
            && !size.starts_with('0')
            && size[..digits].bytes().all(|byte| byte.is_ascii_digit())
            && size[digits..].bytes().all(|byte| b"kKmMgG".contains(&byte)),
        "use an NGINX size such as 20g, not {size:?}"
    );
    Ok(())
}

fn check_prefixes<T>(mounts: &[Mount<T>]) -> Result<()> {
    ensure!(
        !mounts.is_empty(),
        "serve at least one web root or upstream"
    );
    for (index, mount) in mounts.iter().enumerate() {
        ensure!(
            !mounts[..index]
                .iter()
                .any(|other| other.prefix == mount.prefix),
            "path prefix {} is used twice",
            mount.prefix
        );
    }
    Ok(())
}

/// The prefix as a regular expression.
fn pattern(prefix: &str) -> String {
    format!("^{}", prefix.replace('.', r"\."))
}

/// Print a server block, and for a cache its zone, for NGINX's `http` context.
pub(crate) fn nginx_config(server: &WebServer<'_>, config: &WebConfig<'_>) -> Result<String> {
    check_host(server.host)?;
    let certificate = check_path(server.certificate)?;
    let key = check_path(server.key)?;
    let mut out = String::new();
    let mut locations = String::new();
    let prefixes: Vec<&str>;
    match config {
        WebConfig::Static { roots } => {
            check_prefixes(roots)?;
            prefixes = roots.iter().map(|mount| mount.prefix.as_str()).collect();
            for Mount { prefix, source } in roots.iter() {
                let root = check_path(source)?.trim_end_matches('/');
                let regex = pattern(prefix);
                write!(
                    locations,
                    r#"    location = {prefix}metadata/timestamp.json {{
        alias {root}/metadata/timestamp.json;
        {REVALIDATE}
    }}
    location = {prefix}publisher.json {{
        alias {root}/publisher.json;
        {REVALIDATE}
    }}
    location {prefix}metadata/ {{
        if ($uri !~ "{regex}{METADATA}") {{ return 404; }}
        alias {root}/metadata/;
        {IMMUTABLE}
    }}
    location {prefix}targets/ {{
        if ($uri !~ "{regex}{TARGETS}") {{ return 404; }}
        alias {root}/targets/;
        {IMMUTABLE}
    }}
"#
                )?;
            }
        }
        WebConfig::Cache {
            directory,
            upstreams,
            max_size,
            ca_bundle,
        } => {
            check_prefixes(upstreams)?;
            check_size(max_size)?;
            let directory = check_path(directory)?;
            let ca_bundle = check_path(ca_bundle)?;
            prefixes = upstreams
                .iter()
                .map(|mount| mount.prefix.as_str())
                .collect();
            let checked = upstreams
                .iter()
                .map(|mount| check_upstream(mount.source))
                .collect::<Result<Vec<_>>>()?;
            let mut identity = format!("{directory}\n{}", server.host);
            for (mount, upstream) in upstreams.iter().zip(&checked) {
                write!(identity, "\n{}={upstream}", mount.prefix)?;
            }
            // One zone per cache, so several caches can coexist in one NGINX.
            let zone = format!("cps_{}", &digest(identity.as_bytes())[..16]);
            writeln!(
                out,
                "map $upstream_status ${zone}_skip {{ 200 0; default 1; }}"
            )?;
            writeln!(
                out,
                "proxy_cache_path {directory} levels=1:2 keys_zone={zone}:16m max_size={max_size} inactive=30d use_temp_path=off;"
            )?;
            write!(
                locations,
                r#"    proxy_ssl_server_name on;
    proxy_ssl_verify on;
    proxy_ssl_verify_depth 3;
    proxy_ssl_trusted_certificate {ca_bundle};
    proxy_set_header Host $proxy_host;
    proxy_buffering on;
    proxy_connect_timeout 5s;
    proxy_cache_lock on;
    proxy_cache_lock_timeout 20s;
    proxy_cache_lock_age 1m;
    proxy_cache_valid 200 7d;
    proxy_no_cache ${zone}_skip;
    proxy_cache_use_stale off;
    # Query strings never select a different file, so they must not select a different cache entry.
    proxy_cache_key $uri;
"#
            )?;
            for (Mount { prefix, .. }, upstream) in upstreams.iter().zip(&checked) {
                let regex = pattern(prefix);
                write!(
                    locations,
                    r#"    location = {prefix}metadata/timestamp.json {{
        limit_except GET {{ deny all; }}
        proxy_cache off;
        proxy_pass {upstream}/metadata/timestamp.json;
    }}
    location = {prefix}publisher.json {{
        limit_except GET {{ deny all; }}
        proxy_cache off;
        proxy_pass {upstream}/publisher.json;
    }}
    location {prefix}metadata/ {{
        if ($uri !~ "{regex}{METADATA}") {{ return 404; }}
        limit_except GET {{ deny all; }}
        proxy_cache {zone};
        proxy_pass {upstream}/metadata/;
    }}
    location {prefix}targets/ {{
        if ($uri !~ "{regex}{TARGETS}") {{ return 404; }}
        limit_except GET {{ deny all; }}
        proxy_cache {zone};
        proxy_pass {upstream}/targets/;
    }}
"#
                )?;
            }
        }
    }
    writeln!(out, "server {{")?;
    writeln!(out, "    listen 443 ssl;")?;
    writeln!(out, "    server_name {};", server.host)?;
    writeln!(out, "    ssl_certificate {certificate};")?;
    writeln!(out, "    ssl_certificate_key {key};")?;
    writeln!(out, "    autoindex off;")?;
    writeln!(out, "    open_file_cache off;")?;
    out.push_str(&locations);
    // Everything else under each prefix, and on the host, is refused.
    for prefix in prefixes.iter().copied().filter(|prefix| *prefix != "/") {
        writeln!(out, "    location {prefix} {{ return 404; }}")?;
    }
    writeln!(out, "    location / {{ return 404; }}")?;
    writeln!(out, "}}")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> WebServer<'static> {
        WebServer {
            host: "packages.example.org",
            certificate: Path::new("/etc/tls/cert.pem"),
            key: Path::new("/etc/tls/key.pem"),
        }
    }

    fn mount<T>(prefix: &str, source: T) -> Mount<T> {
        Mount {
            prefix: prefix.to_owned(),
            source,
        }
    }

    #[test]
    fn mounts_take_an_optional_prefix() -> Result<()> {
        assert_eq!(
            Mount::parse("/srv/cps-www/canopa")?,
            mount("/", "/srv/cps-www/canopa".to_owned())
        );
        assert_eq!(
            Mount::parse("/canopa=https://origin.example/canopa")?,
            mount("/canopa/", "https://origin.example/canopa".to_owned())
        );
        for bad in ["canopa=/srv/x", "/a b=/srv/x", "/../x=/srv/x", "//x=/srv/x"] {
            assert!(Mount::parse(bad).is_err(), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn caches_key_on_the_path_and_get_distinct_zones() -> Result<()> {
        let cache = |directory: &'static str, upstream: &'static str| {
            nginx_config(
                &server(),
                &WebConfig::Cache {
                    directory: Path::new(directory),
                    upstreams: &[mount("/", upstream)],
                    max_size: "5g",
                    ca_bundle: Path::new("/etc/ssl/certs/ca-certificates.crt"),
                },
            )
        };
        let first = cache("/var/cache/nginx/canopa", "https://origin.example/canopa")?;
        assert!(first.contains("proxy_cache_key $uri;"));
        assert!(first.contains("max_size=5g"));
        assert!(
            first.contains("proxy_pass https://origin.example/canopa/metadata/timestamp.json;")
        );
        let second = cache("/var/cache/nginx/runtime", "https://origin.example/runtime")?;
        let zone = |config: &str| {
            config
                .split("keys_zone=")
                .nth(1)
                .and_then(|rest| rest.split(':').next())
                .unwrap()
                .to_owned()
        };
        assert_ne!(zone(&first), zone(&second));
        assert!(cache("/var/cache/nginx/x", "http://origin.example/x").is_err());
        assert!(cache("/var/cache/nginx/x", "https://origin.example/x/").is_err());
        assert!(cache("/var/cache/nginx x", "https://origin.example/x").is_err());
        // Only a host name, an IP address, and a port reach proxy_pass.
        for bad in [
            "https://origin;x.example/x",
            "https://origin{x}.example/x",
            "https://origin$host.example/x",
            "https://-origin.example/x",
            "https://origin.example//x",
        ] {
            assert!(cache("/var/cache/nginx/x", bad).is_err(), "{bad}");
        }
        assert!(
            cache("/var/cache/nginx/x", "https://[::1]:8443/x")?
                .contains("proxy_pass https://[::1]:8443/x/targets/;")
        );
        Ok(())
    }

    #[test]
    fn one_server_serves_several_publishers_under_prefixes() -> Result<()> {
        let roots = [
            mount("/canopa/", Path::new("/srv/cps-www/canopa")),
            mount("/runtime.v2/", Path::new("/srv/cps-www/runtime")),
        ];
        let config = nginx_config(&server(), &WebConfig::Static { roots: &roots })?;
        assert_eq!(config.matches("server {").count(), 1);
        assert!(config.contains("location = /canopa/metadata/timestamp.json {"));
        assert!(config.contains("alias /srv/cps-www/canopa/metadata/timestamp.json;"));
        assert!(config.contains(r#"if ($uri !~ "^/runtime\.v2/targets/[a-f0-9]{64}"#));
        assert!(config.contains("alias /srv/cps-www/runtime/targets/;"));
        assert!(config.contains("location /canopa/ { return 404; }"));
        assert!(config.contains("location / { return 404; }"));
        let twice = [
            mount("/a/", Path::new("/srv/a")),
            mount("/a/", Path::new("/srv/b")),
        ];
        assert!(nginx_config(&server(), &WebConfig::Static { roots: &twice }).is_err());
        Ok(())
    }

    #[test]
    fn static_servers_serve_only_cps_files() -> Result<()> {
        let roots = [mount("/", Path::new("/srv/cps/canopa"))];
        let config = nginx_config(&server(), &WebConfig::Static { roots: &roots })?;
        assert!(config.contains("alias /srv/cps/canopa/metadata/;"));
        assert!(config.contains("location = /metadata/timestamp.json"));
        assert_eq!(config.matches("location / { return 404; }").count(), 1);
        assert!(
            nginx_config(
                &WebServer {
                    host: "bad host",
                    ..server()
                },
                &WebConfig::Static { roots: &roots },
            )
            .is_err()
        );
        Ok(())
    }
}
