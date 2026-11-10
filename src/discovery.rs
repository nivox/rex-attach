use crate::protocol::Control;
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsString, future::Future, path::PathBuf, time::Duration};

/// Resolve once at startup, keeping the validated connection for the application.
pub async fn connect(explicit: Option<&str>) -> Result<(String, Control)> {
    connect_with(explicit, |name| std::env::var_os(name)).await
}

async fn connect_with(
    explicit: Option<&str>,
    mut env: impl FnMut(&str) -> Option<OsString>,
) -> Result<(String, Control)> {
    if let Some(path) = explicit {
        return connect_selected(path, "--socket").await;
    }
    if let Some(server) = env("REX_SERVER").filter(|value| !value.is_empty()) {
        let server = server.to_str().context("REX_SERVER is not valid UTF-8")?;
        return connect_selected(&server_path(server)?, "REX_SERVER").await;
    }
    probe(candidates(env), |path| async move {
        tokio::time::timeout(Duration::from_secs(2), Control::connect(&path))
            .await
            .context("Rex handshake timed out after 2 seconds")?
    })
    .await
}

async fn connect_selected(path: &str, source: &str) -> Result<(String, Control)> {
    ensure!(!path.is_empty(), "{source} must not be empty");
    let control = Control::connect(path)
        .await
        .with_context(|| format!("connecting to {path} selected by {source}"))?;
    Ok((path.to_owned(), control))
}

fn server_path(server: &str) -> Result<String> {
    let path = if let Some(encoded) = server.strip_prefix("unix://") {
        // percent_decode_str leaves malformed escapes unchanged; reject those explicitly.
        for escape in encoded.split('%').skip(1) {
            ensure!(
                escape.len() >= 2 && escape.as_bytes()[..2].iter().all(u8::is_ascii_hexdigit),
                "REX_SERVER contains an invalid percent escape; pass --socket PATH"
            );
        }
        percent_encoding::percent_decode_str(encoded)
            .decode_utf8()
            .context("REX_SERVER socket path is not valid UTF-8")?
            .into_owned()
    } else {
        server.to_owned()
    };
    ensure!(
        path.starts_with('/') && !path.contains('\0'),
        "REX_SERVER must be an absolute Unix socket path or unix:///path address; host labels and network endpoints are unsupported; pass --socket PATH"
    );
    Ok(path)
}

fn candidates(mut env: impl FnMut(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut directory = |name| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let mut paths = Vec::new();
    if let Some(runtime) = directory("REX_RUNTIME_DIR") {
        paths.push(runtime.join("server.sock"));
    }
    if let Some(runtime) = directory("XDG_RUNTIME_DIR") {
        paths.push(runtime.join("rex/server.sock"));
    }
    #[cfg(target_os = "linux")]
    // SAFETY: geteuid has no preconditions and does not modify process state.
    paths.push(PathBuf::from(format!(
        "/run/user/{}/rex/server.sock",
        unsafe { libc::geteuid() }
    )));
    if let Some(state) = directory("XDG_STATE_HOME")
        .or_else(|| directory("HOME").map(|home| home.join(".local/state")))
    {
        paths.push(state.join("rex/run/server.sock"));
    }
    paths.push(PathBuf::from("/run/rex/server.sock"));
    paths
}

async fn probe<T, F, Fut>(paths: Vec<PathBuf>, mut connect: F) -> Result<(String, T)>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    let mut failures = Vec::new();
    for path in paths {
        // Resolve directory and socket symlinks so aliases do not create ambiguity.
        let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.insert(identity) {
            continue;
        }
        let Some(path) = path.to_str() else {
            failures.push(format!("{}: path is not valid UTF-8", path.display()));
            continue;
        };
        match connect(path.to_owned()).await {
            Ok(control) => found.push((path.to_owned(), control)),
            Err(error) => failures.push(format!("{path}: {error:#}")),
        }
    }
    match found.len() {
        0 => bail!(
            "unable to discover a valid Rex socket; pass --socket PATH\nTried:\n  {}",
            failures.join("\n  ")
        ),
        1 => Ok(found.pop().unwrap()),
        _ => bail!(
            "multiple Rex servers discovered; select one with --socket PATH:\n  {}",
            found
                .iter()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn welcome(
        listener: tokio::net::UnixListener,
        revision: u64,
    ) -> Result<crate::protocol::Socket> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{
            Message,
            handshake::server::{ErrorResponse, Request, Response},
        };

        // Tungstenite's callback signature requires this unboxed error type.
        #[allow(clippy::result_large_err)]
        fn negotiate(
            _: &Request,
            mut response: Response,
        ) -> std::result::Result<Response, ErrorResponse> {
            response
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", "rex.control.v1".parse().unwrap());
            Ok(response)
        }

        let (stream, _) = listener.accept().await?;
        let stream: Box<dyn crate::protocol::Io> = Box::new(stream);
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, negotiate).await?;
        let hello = crate::protocol::next_json(&mut socket).await?;
        assert_eq!(hello["type"], "hello");
        socket
            .send(Message::text(
                serde_json::json!({"type":"welcome", "api_revision":revision}).to_string(),
            ))
            .await?;
        Ok(socket)
    }

    #[tokio::test]
    async fn validates_real_sockets_and_reuses_connection_through_symlink_alias() -> Result<()> {
        use tokio::net::UnixListener;

        let directory = TestDirectory(std::env::temp_dir().join(format!(
            "rex-discovery-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
        )));
        std::fs::create_dir(&directory.0)?;
        let valid = directory.0.join("valid.sock");
        let wrong = directory.0.join("wrong.sock");
        let stale = directory.0.join("stale.sock");
        let alias = directory.0.join("alias.sock");
        let listener = UnixListener::bind(&valid)?;
        let wrong_listener = UnixListener::bind(&wrong)?;
        drop(UnixListener::bind(&stale)?);
        std::os::unix::fs::symlink(&valid, &alias)?;
        let server = async {
            let mut socket = welcome(listener, 11).await?;
            let request = crate::protocol::next_json(&mut socket).await?;
            assert_eq!(request["method"], "server.status");
            crate::test_support::reply(&mut socket, &request, serde_json::json!({"ok":true}), false)
                .await
        };
        let wrong_server = async { welcome(wrong_listener, 99).await.map(|_| ()) };
        let client = async {
            let (path, mut control) = probe(
                vec![stale, wrong, valid.clone(), alias],
                |path| async move { Control::connect(&path).await },
            )
            .await?;
            assert_eq!(path, valid.to_str().unwrap());
            let status = control.call("server.status", serde_json::json!({})).await?;
            assert_eq!(status["ok"], true);
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(server, wrong_server, client)
        })
        .await??;
        Ok(())
    }

    #[test]
    fn accepts_paths_and_decodes_only_unix_uris() -> Result<()> {
        assert_eq!(
            server_path("/tmp/rex%20server.sock")?,
            "/tmp/rex%20server.sock"
        );
        assert_eq!(
            server_path("unix:///tmp/rex%20server.sock")?,
            "/tmp/rex server.sock"
        );
        for invalid in [
            "studio",
            "tcp://localhost:1234",
            "https://rex.example",
            "relative.sock",
            "unix://",
            "unix://host/path",
            "unix:///a%",
            "unix:///a%GG",
            "unix:///a%00",
            "unix:///a%FF",
        ] {
            assert!(server_path(invalid).is_err(), "{invalid}");
        }
        Ok(())
    }

    #[test]
    fn uses_runtime_overrides_and_state_fallbacks() {
        let paths = candidates(|name| match name {
            "REX_RUNTIME_DIR" => Some("/custom".into()),
            "XDG_RUNTIME_DIR" => Some("/runtime".into()),
            "XDG_STATE_HOME" => Some("/state".into()),
            "HOME" => Some("/home/test".into()),
            _ => None,
        });
        assert_eq!(paths[0], PathBuf::from("/custom/server.sock"));
        assert_eq!(paths[1], PathBuf::from("/runtime/rex/server.sock"));
        assert!(paths.contains(&PathBuf::from("/state/rex/run/server.sock")));
        assert!(!paths.contains(&PathBuf::from(
            "/home/test/.local/state/rex/run/server.sock"
        )));
        let paths = candidates(|name| match name {
            "HOME" => Some("/home/test".into()),
            _ => Some("".into()),
        });
        assert!(paths.contains(&PathBuf::from(
            "/home/test/.local/state/rex/run/server.sock"
        )));
        assert_eq!(
            paths.last().unwrap(),
            &PathBuf::from("/run/rex/server.sock")
        );
    }

    #[tokio::test]
    async fn probes_past_failures_deduplicates_and_keeps_valid_connection() -> Result<()> {
        let mut attempts = Vec::new();
        let (path, connection) = probe(
            ["/missing", "/stale", "/valid", "/valid"]
                .map(PathBuf::from)
                .to_vec(),
            |path| {
                attempts.push(path.clone());
                async move {
                    if path == "/valid" {
                        Ok(42)
                    } else {
                        bail!("connection refused")
                    }
                }
            },
        )
        .await?;
        assert_eq!(path, "/valid");
        assert_eq!(connection, 42);
        assert_eq!(attempts, ["/missing", "/stale", "/valid"]);
        Ok(())
    }

    #[tokio::test]
    async fn reports_failed_candidates_and_ambiguous_servers() {
        let error = probe(vec!["/stale".into()], |_| async {
            Err::<(), _>(anyhow::anyhow!("handshake rejected"))
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("--socket PATH"));
        assert!(error.contains("/stale: handshake rejected"));
        let error = probe(vec!["/first".into(), "/second".into()], |_| async {
            Ok(())
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("multiple Rex servers"));
        assert!(error.contains("/first"));
        assert!(error.contains("/second"));
    }

    #[tokio::test]
    async fn explicit_selections_never_fall_back() {
        let result = connect_with(Some(""), |_| {
            panic!("explicit socket must override environment")
        })
        .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("--socket must not be empty")
        );
        for server in [
            "tcp://localhost:1234",
            "/rex-attach-nonexistent/server.sock",
        ] {
            let result = connect_with(None, |name| {
                assert_eq!(name, "REX_SERVER", "must not probe fallback directories");
                Some(server.into())
            })
            .await;
            assert!(result.err().unwrap().to_string().contains("REX_SERVER"));
        }
    }
}
