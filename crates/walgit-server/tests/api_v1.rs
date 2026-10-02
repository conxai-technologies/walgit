#![allow(clippy::many_single_char_names)]
//! `/api/v1` (D20): the versioned programmatic surface, its browser-lane alias
//! (`/api-browser`), CORS for foreign origins, discovery, `me`, repo summary and
//! admin, and the SDK artefact route.

mod harness;

use harness::{Server, git_in};
use serde_json::Value;

type TestResult = anyhow::Result<()>;

async fn req(
    server: &Server,
    method: reqwest::Method,
    path: &str,
    extra: &[(&str, &str)],
) -> anyhow::Result<(reqwest::StatusCode, String, reqwest::header::HeaderMap)> {
    let mut r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .request(method, format!("{}{path}", server.base_url))
        .header("Accept", "application/json");
    for (k, v) in extra {
        r = r.header(*k, *v);
    }
    let resp = r.send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    Ok((status, resp.text().await?, headers))
}
async fn req_body(
    server: &Server,
    method: reqwest::Method,
    path: &str,
    extra: &[(&str, &str)],
    body: &'static str,
) -> anyhow::Result<(reqwest::StatusCode, String)> {
    let mut r = reqwest::Client::new()
        .request(method, format!("{}{path}", server.base_url))
        .header("Accept", "application/json")
        .body(body);
    for (k, v) in extra {
        r = r.header(*k, *v);
    }
    let resp = r.send().await?;
    let status = resp.status();
    Ok((status, resp.text().await?))
}
fn hdr(h: &reqwest::header::HeaderMap, k: &str) -> String {
    h.get(k)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}
async fn json(server: &Server, path: &str) -> anyhow::Result<Value> {
    let (st, text, _) = req(server, reqwest::Method::GET, path, &[]).await?;
    anyhow::ensure!(st.is_success(), "GET {path} -> {st}: {text}");
    Ok(serde_json::from_str(&text)?)
}

fn fixture(server: &Server) -> anyhow::Result<String> {
    let dir = tempfile::tempdir()?.keep();
    git_in(&dir, &["init", "-q", "-b", "main"])?;
    git_in(&dir, &["config", "user.email", "t@t"])?;
    git_in(&dir, &["config", "user.name", "Tester"])?;
    std::fs::write(dir.join("README.md"), "# v1\n")?;
    git_in(&dir, &["add", "."])?;
    git_in(
        &dir,
        &[
            "commit",
            "-q",
            "-m",
            "initial\n\nSee https://github.com/o/r/pull/7 for context.\n\nMerge-Queue-Phase: target-publish\nMerge-Queue-Pull-Request: 7\nCo-authored-by: Jane <jane@example.com>",
        ],
    )?;
    git_in(
        &dir,
        &[
            "-c",
            "tag.forceSignAnnotated=false",
            "-c",
            "tag.gpgsign=false",
            "tag",
            "v1",
        ],
    )?;
    git_in(&dir, &["branch", "feature/x"])?;
    git_in(
        &dir,
        &["push", "-q", "--mirror", &server.repo_url("o", "r")],
    )?;
    Ok(git_in(&dir, &["rev-parse", "HEAD"])?.trim().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v1_surface_and_browser_lane() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.cors_origins = vec!["https://*.docs.example.com".into()];
    })
    .await?;
    server.put_repo("o", "r").await?;
    let head = fixture(&server)?;

    // discovery
    let d = json(&server, "/api/v1").await?;
    assert_eq!(d["version"], 1);
    assert!(d["sdk"].as_str().unwrap().ends_with("/repos.js"));
    assert!(
        d["browser_base"]
            .as_str()
            .unwrap()
            .ends_with("/api-browser/v1")
    );
    assert!(
        d["auth"]["authenticate"]
            .as_str()
            .unwrap()
            .ends_with("/api-browser/v1/authenticate")
    );
    // `docs` is this host's API page, derived from the same base as every
    // other URL in the document (AGENTS.md §5: no hardcoded hostnames).
    let base = d["base"].as_str().unwrap();
    assert_eq!(
        d["docs"],
        format!("{}/api", base.trim_end_matches("/api/v1")),
        "{d}"
    );

    // me (auth mode none in tests → anonymous principal)
    let (st, _, h) = req(&server, reqwest::Method::GET, "/api/v1/me", &[]).await?;
    assert_eq!(st, 200);
    assert_eq!(hdr(&h, "cache-control"), "no-store");

    // owners
    assert_eq!(
        json(&server, "/api/v1/owners").await?,
        serde_json::json!(["o"])
    );
    assert_eq!(
        json(&server, "/api/v1/owners/o/repos").await?,
        serde_json::json!(["r"])
    );
    assert_eq!(
        json(&server, "/api/v1/owners/nobody/repos").await?,
        serde_json::json!([])
    );

    // repo summary: SWR + ETag on head
    let (st, text, h) = req(&server, reqwest::Method::GET, "/o/r/api", &[]).await?;
    assert_eq!(st, 200, "{text}");
    let s: Value = serde_json::from_str(&text)?;
    assert_eq!(s["full_name"], "o/r");
    assert_eq!(s["head"]["name"], "main");
    assert_eq!(s["head"]["sha"], head);
    assert_eq!(s["branches"], 2);
    assert_eq!(s["tags"], 1);
    assert!(s["clone_url"].as_str().unwrap().ends_with("/o/r.git"));
    assert!(s["api_url"].as_str().unwrap().ends_with("/o/r/api"));
    assert_eq!(hdr(&h, "etag"), format!("\"{head}\""));
    assert!(hdr(&h, "cache-control").contains("stale-while-revalidate"));
    let (st, _, _) = req(
        &server,
        reqwest::Method::GET,
        "/o/r/api",
        &[("If-None-Match", &format!("\"{head}\""))],
    )
    .await?;
    assert_eq!(st, 304);
    assert_eq!(
        req(&server, reqwest::Method::GET, "/o/nope/api", &[])
            .await?
            .0,
        404
    );

    // the repo-scoped read endpoints are the same handlers as /{o}/{r}/api/…
    let refs = json(&server, "/o/r/api/refs").await?;
    assert_eq!(refs["head"]["sha"], head);
    let r = json(&server, "/o/r/api/resolve/feature/x").await?;
    assert_eq!(r["kind"], "branch");
    let t = json(&server, &format!("/o/r/api/tree/{head}")).await?;
    assert_eq!(t["entries"][0]["name"], "README.md");
    let b = json(&server, &format!("/o/r/api/blob/{head}/README.md")).await?;
    assert_eq!(b["contents"], "# v1\n");
    let c = json(&server, &format!("/o/r/api/commits?ref={head}")).await?;
    assert_eq!(c["commits"][0]["subject"], "initial");
    let c = json(&server, &format!("/o/r/api/commit/{head}")).await?;
    assert_eq!(c["commit"]["sha"], head);
    // Trailers split off the body (git interpret-trailers rules); body keeps the prose + URL.
    assert_eq!(
        c["commit"]["body"],
        "See https://github.com/o/r/pull/7 for context."
    );
    assert_eq!(
        c["commit"]["trailers"][1]["key"],
        "Merge-Queue-Pull-Request"
    );
    assert_eq!(c["commit"]["trailers"][1]["value"], "7");
    assert_eq!(c["commit"]["trailers"].as_array().unwrap().len(), 3);
    let tags = json(&server, "/o/r/api/refs/tags").await?;
    assert_eq!(tags["refs"][0]["name"], "v1");
    let tasks = json(&server, "/o/r/api/tasks").await?;
    assert!(tasks["running"].is_array());
    let (st, _, _) = req(&server, reqwest::Method::GET, "/o/r/api/overview", &[]).await?;
    assert_eq!(st, 200);

    // browser lane: /{o}/{r}/api-browser/… is the same surface (query preserved)
    let (st, text, _) = req(
        &server,
        reqwest::Method::GET,
        &format!("/o/r/api-browser/commits?ref={head}&n=1"),
        &[],
    )
    .await?;
    assert_eq!(st, 200, "{text}");
    let c: Value = serde_json::from_str(&text)?;
    assert_eq!(c["commits"].as_array().unwrap().len(), 1);

    // CORS: allowed wildcard origin gets credentials; foreign origin gets nothing; preflight is open.
    let (st, _, h) = req(
        &server,
        reqwest::Method::GET,
        "/o/r/api/refs",
        &[("Origin", "https://wiki.docs.example.com")],
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(
        hdr(&h, "access-control-allow-origin"),
        "https://wiki.docs.example.com"
    );
    assert_eq!(hdr(&h, "access-control-allow-credentials"), "true");
    assert!(hdr(&h, "access-control-expose-headers").contains("ETag"));
    assert!(
        h.get_all("vary")
            .iter()
            .any(|v| v.to_str().unwrap().contains("Origin"))
    );
    let (st, _, h) = req(
        &server,
        reqwest::Method::GET,
        "/o/r/api/refs",
        &[("Origin", "https://evil.example")],
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(hdr(&h, "access-control-allow-origin"), "");
    let (st, _, h) = req(
        &server,
        reqwest::Method::OPTIONS,
        "/o/r/api-browser/refs",
        &[
            ("Origin", "https://x.docs.example.com"),
            ("Access-Control-Request-Method", "GET"),
            ("Access-Control-Request-Headers", "authorization"),
        ],
    )
    .await?;
    assert_eq!(st, 204);
    assert!(hdr(&h, "access-control-allow-methods").contains("GET"));
    assert!(
        hdr(&h, "access-control-allow-headers")
            .to_ascii_lowercase()
            .contains("authorization")
    );
    // a state-changing call from a foreign origin is refused before it reaches a handler
    let (st, _, _) = req(
        &server,
        reqwest::Method::DELETE,
        "/o/r/api/policy",
        &[("Origin", "https://evil.example")],
    )
    .await?;
    assert_eq!(st, 403);
    // non-API paths never get CORS headers
    let (_, _, h) = req(
        &server,
        reqwest::Method::GET,
        "/o/r.git/info/refs?service=git-upload-pack",
        &[("Origin", "https://x.docs.example.com")],
    )
    .await?;
    assert_eq!(hdr(&h, "access-control-allow-origin"), "");
    // the browser lane is the same surface under /api-browser (D27)
    let (st, _, h) = req(
        &server,
        reqwest::Method::GET,
        "/o/r/api-browser/refs",
        &[("Origin", "https://x.docs.example.com")],
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(
        hdr(&h, "access-control-allow-origin"),
        "https://x.docs.example.com"
    );
    // the lane-first forms are gone (banner: no aliases)
    for gone in [
        "/api/v1/repos/o/r",
        "/api/v1/repos/o/r/refs",
        "/api-browser/v1/repos/o/r/refs",
        "/services/api/o/r/refs",
    ] {
        assert_eq!(
            req(&server, reqwest::Method::GET, gone, &[]).await?.0,
            404,
            "{gone} must be gone"
        );
    }

    // policy + repo admin under the repo prefix
    let (st, _, _) = req(&server, reqwest::Method::GET, "/o/r/api/policy", &[]).await?;
    assert_eq!(st, 200);
    let (st, _, _) = req(&server, reqwest::Method::PUT, "/o/new/api", &[]).await?;
    assert!(st.is_success(), "{st}");
    assert_eq!(
        json(&server, "/api/v1/owners/o/repos").await?,
        serde_json::json!(["new", "r"])
    );
    let (st, _, _) = req(&server, reqwest::Method::DELETE, "/o/new/api", &[]).await?;
    assert!(st.is_success(), "{st}");
    assert_eq!(
        json(&server, "/api/v1/owners/o/repos").await?,
        serde_json::json!(["r"])
    );

    // authenticate: anonymous mode is "signed in" → the popup page
    let (st, text, h) = req(
        &server,
        reqwest::Method::GET,
        "/api-browser/v1/authenticate",
        &[],
    )
    .await?;
    assert_eq!(st, 200, "{text}");
    assert!(hdr(&h, "content-type").starts_with("text/html"));
    assert!(text.contains("repos:authenticated"));

    // the SDK artefacts (built into web/dist by `pnpm run build`) at their permanent URLs
    for name in ["/repos.js", "/repos.mjs"] {
        let (st, body, h) = req(&server, reqwest::Method::GET, name, &[]).await?;
        assert_eq!(st, 200, "{name}");
        assert!(
            hdr(&h, "content-type").starts_with("text/javascript"),
            "{name}"
        );
        assert_eq!(hdr(&h, "cache-control"), "no-cache");
        assert!(!hdr(&h, "etag").is_empty());
        // D27: the SDK puts the lane after the repository (`/o/r/api` | `/o/r/api-browser`) and
        // opens `/api-browser/v1/authenticate`; it never emits the deleted lane-first forms.
        assert!(
            body.contains("/api-browser/v1/authenticate") && body.contains("repos:authenticated"),
            "{name} is not the SDK"
        );
        assert!(
            !body.contains("/v1/repos") && !body.contains("services/api/"),
            "{name} emits a deleted lane-first form"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_cors_without_config() -> TestResult {
    let server = Server::start().await?;
    let (st, _, h) = req(
        &server,
        reqwest::Method::GET,
        "/api/v1/owners",
        &[("Origin", "https://x.docs.example.com")],
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(hdr(&h, "access-control-allow-origin"), "");
    let (st, _, h) = req(
        &server,
        reqwest::Method::OPTIONS,
        "/api/v1/owners",
        &[
            ("Origin", "https://x.docs.example.com"),
            ("Access-Control-Request-Method", "GET"),
        ],
    )
    .await?;
    assert_eq!(st, 204);
    assert_eq!(hdr(&h, "access-control-allow-origin"), "");
    Ok(())
}

/// D26/D27: everything of a repository under its own prefix — the
/// admin/settings surface at `/{o}/{r}/api[/policy|/settings…]`, and the
/// same under the browser lane `/{o}/{r}/api-browser/…`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn d26_prefix_form_matches_v1_alias() -> TestResult {
    let server = Server::start().await?;
    let c = reqwest::Client::new();
    // create via the prefix form
    assert_eq!(
        c.put(format!("{}/t/pfx/api", server.base_url))
            .send()
            .await?
            .status(),
        201
    );
    let a: serde_json::Value = c
        .get(format!("{}/t/pfx/api", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    let b: serde_json::Value = c
        .get(format!("{}/t/pfx/api-browser", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(a["full_name"], "t/pfx");
    assert_eq!(a["full_name"], b["full_name"]);
    // settings + policy through the prefix form
    let r = c
        .put(format!(
            "{}/t/pfx/api/settings?message=via+prefix",
            server.base_url
        ))
        .body("[packs]\nfold_when_fresh_packs_reach = 7\n")
        .send()
        .await?;
    assert_eq!(r.status(), 200, "{}", r.text().await?);
    let s: serde_json::Value = c
        .get(format!("{}/t/pfx/api-browser/settings", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(s["revision"], 1);
    assert_eq!(s["message"], "via prefix");
    let d: serde_json::Value = c
        .get(format!("{}/t/pfx/api/settings/describe", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(d["packs"]["fold_when_fresh_packs_reach"], 7);
    let p: serde_json::Value = c
        .get(format!("{}/t/pfx/api/policy", server.base_url))
        .send()
        .await?
        .json()
        .await?;
    assert!(p.is_object());
    let v: serde_json::Value = c
        .post(format!("{}/t/pfx/api/policy/validate", server.base_url))
        .body("{}")
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(v["ok"], true);
    // refs etc. (already D15)
    assert_eq!(
        c.get(format!("{}/t/pfx/api/refs", server.base_url))
            .send()
            .await?
            .status(),
        200
    );
    // delete via the prefix form
    assert_eq!(
        c.delete(format!("{}/t/pfx/api", server.base_url))
            .send()
            .await?
            .status(),
        204
    );
    assert_eq!(
        c.get(format!("{}/t/pfx/api", server.base_url))
            .send()
            .await?
            .status(),
        404
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_delete_requires_admin() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "writer".into(),
                token: "writer-token".into(),
                token_env: None,
                write: true,
                admin: false,
            },
            walgit_config::StaticToken {
                principal: "admin".into(),
                token: "admin-token".into(),
                token_env: None,
                write: true,
                admin: true,
            },
        ];
    })
    .await?;

    let writer = [("Authorization", "Bearer writer-token")];
    let admin = [("Authorization", "Bearer admin-token")];

    assert_eq!(
        req(&server, reqwest::Method::PUT, "/secure/delete/api", &writer,)
            .await?
            .0,
        201,
        "write permission still creates repositories"
    );

    for path in [
        "/secure/delete",
        "/secure/delete/api",
        "/secure/delete/api-browser",
    ] {
        assert_eq!(
            req(&server, reqwest::Method::DELETE, path, &writer)
                .await?
                .0,
            403,
            "non-admin deletion through {path}"
        );
    }
    assert_eq!(
        req(&server, reqwest::Method::GET, "/secure/delete/api", &writer,)
            .await?
            .0,
        200,
        "forbidden deletion must leave the repository intact"
    );

    assert_eq!(
        req(
            &server,
            reqwest::Method::DELETE,
            "/secure/delete/api",
            &admin,
        )
        .await?
        .0,
        204
    );
    assert_eq!(
        req(&server, reqwest::Method::GET, "/secure/delete/api", &admin,)
            .await?
            .0,
        404
    );
    Ok(())
}

/// D24 and API.md §5: on the JSON surface a write token creates repositories,
/// but the policy and settings documents move only with admin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_and_settings_writes_require_admin() -> TestResult {
    const POLICY: &str = r#"{"version":1,"groups":[],"rules":[]}"#;
    const SETTINGS: &str = "[packs]\nfold_when_fresh_packs_reach = 3\n";
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "writer".into(),
                token: "writer-token".into(),
                token_env: None,
                write: true,
                admin: false,
            },
            walgit_config::StaticToken {
                principal: "admin".into(),
                token: "admin-token".into(),
                token_env: None,
                write: true,
                admin: true,
            },
        ];
    })
    .await?;
    let writer = [("Authorization", "Bearer writer-token")];
    let admin = [("Authorization", "Bearer admin-token")];
    assert_eq!(
        req(&server, reqwest::Method::PUT, "/gates/repo/api", &writer)
            .await?
            .0,
        201,
        "write permission creates the repository"
    );

    for (path, body) in [
        ("/gates/repo/api/policy", POLICY),
        ("/gates/repo/api/settings", SETTINGS),
    ] {
        let (st, text) = req_body(&server, reqwest::Method::PUT, path, &writer, body).await?;
        assert_eq!(st, 403, "a write token must not PUT {path}: {text}");
        assert_eq!(
            req(&server, reqwest::Method::DELETE, path, &writer)
                .await?
                .0,
            403,
            "a write token must not DELETE {path}"
        );
    }
    let (st, text) = req_body(
        &server,
        reqwest::Method::PUT,
        "/gates/repo/api/policy",
        &admin,
        POLICY,
    )
    .await?;
    assert_eq!(st, 204, "{text}");
    let (st, text) = req_body(
        &server,
        reqwest::Method::PUT,
        "/gates/repo/api/settings",
        &admin,
        SETTINGS,
    )
    .await?;
    assert_eq!(st, 200, "{text}");
    Ok(())
}

/// D50: a gateway that verified the user vouches for it with its own `trusted_forwarders`
/// token (no new auth mode). The token is the ceiling; `X-Walgit-Access` narrows per
/// request and `X-Walgit-Owners` narrows what exists: listings omit other owners and every
/// route under their prefix — JSON API in both lanes, repo admin, UI data, git smart HTTP,
/// LFS — answers the 404 of a repository that does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trusted_forwarder_narrows_access_and_owners_below_its_own_token() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![walgit_config::StaticToken {
            principal: "gw".into(),
            // As a Kubernetes Secret often holds it.
            token: "gw-token\n".into(),
            token_env: None,
            write: true,
            admin: true,
        }];
        c.server.auth.trusted_forwarders = vec!["gw".into()];
    })
    .await?;
    let as_ = |access: &'static str, owners: Option<&'static str>| {
        let mut h = vec![
            ("Authorization", "Bearer gw-token"),
            ("X-Walgit-Principal", "dev@example.com"),
            ("X-Walgit-Access", access),
        ];
        if let Some(o) = owners {
            h.push(("X-Walgit-Owners", o));
        }
        h
    };
    for path in ["/acme/app/api", "/other/app/api"] {
        let (st, text, _) = req(&server, reqwest::Method::PUT, path, &as_("write", None)).await?;
        assert_eq!(st, 201, "{path}: {text}");
    }
    let (st, me, _) = req(
        &server,
        reqwest::Method::GET,
        "/api/v1/me",
        &as_("read", None),
    )
    .await?;
    assert_eq!(st, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&me)?["principal"],
        "dev@example.com"
    );
    // Read cannot create; write cannot delete; admin (within the token's ceiling) can.
    let (st, _, _) = req(
        &server,
        reqwest::Method::PUT,
        "/acme/new/api",
        &as_("read", None),
    )
    .await?;
    assert_eq!(st, 403);
    let (st, _, _) = req(
        &server,
        reqwest::Method::DELETE,
        "/acme/app/api",
        &as_("write", None),
    )
    .await?;
    assert_eq!(st, 403, "write is push, not admin");
    let (st, text, _) = req(
        &server,
        reqwest::Method::PUT,
        "/acme/tmp/api",
        &as_("write", None),
    )
    .await?;
    assert_eq!(st, 201, "{text}");
    let (st, text, _) = req(
        &server,
        reqwest::Method::DELETE,
        "/acme/tmp/api",
        &as_("admin", None),
    )
    .await?;
    assert!(st.is_success(), "{st}: {text}");

    let scoped = as_("admin", Some("acme"));
    for path in ["/api/v1/owners", "/services/api/owners"] {
        let (st, text, _) = req(&server, reqwest::Method::GET, path, &scoped).await?;
        assert_eq!(st, 200, "{path}");
        assert_eq!(
            serde_json::from_str::<Value>(&text)?,
            serde_json::json!(["acme"]),
            "{path}"
        );
    }
    for path in ["/api/v1/owners/other/repos", "/services/api/owners/other"] {
        let (st, text, _) = req(&server, reqwest::Method::GET, path, &scoped).await?;
        assert_eq!((st.as_u16(), text.as_str()), (200, "[]"), "{path}");
    }
    for (method, path) in [
        (reqwest::Method::GET, "/other/app/api"),
        (reqwest::Method::GET, "/other/app/api-browser"),
        (reqwest::Method::GET, "/other/app/api/refs"),
        (reqwest::Method::GET, "/other/app/api/settings"),
        (reqwest::Method::GET, "/other/app/api/policy"),
        (
            reqwest::Method::GET,
            "/other/app.git/info/refs?service=git-upload-pack",
        ),
        (
            reqwest::Method::POST,
            "/other/app.git/info/lfs/objects/batch",
        ),
        (reqwest::Method::DELETE, "/other/app/api"),
        (reqwest::Method::PUT, "/other/fresh/api"),
    ] {
        let (st, text, _) = req(&server, method.clone(), path, &scoped).await?;
        assert_eq!(st, 404, "{method} {path} is out of scope: {text}");
    }
    assert_eq!(
        req(&server, reqwest::Method::GET, "/acme/app/api", &scoped)
            .await?
            .0,
        200
    );
    let (st, _, h) = req(
        &server,
        reqwest::Method::GET,
        "/acme/app.git/info/refs?service=git-upload-pack",
        &scoped,
    )
    .await?;
    assert_eq!(st, 200);
    assert!(hdr(&h, "content-type").contains("git-upload-pack"));
    // The out-of-scope repository is intact for a caller without the scope.
    assert_eq!(
        req(
            &server,
            reqwest::Method::GET,
            "/other/app/api",
            &as_("read", None)
        )
        .await?
        .0,
        200
    );
    // Without the forwarding headers the gateway is itself (its own token), as before.
    let (_, me, _) = req(
        &server,
        reqwest::Method::GET,
        "/api/v1/me",
        &[("Authorization", "Bearer gw-token")],
    )
    .await?;
    assert_eq!(serde_json::from_str::<Value>(&me)?["principal"], "gw");
    Ok(())
}

/// A forwarded request whose forwarder credential is wrong or missing, or narrowing headers
/// from a caller that cannot narrow, is the gateway's misconfiguration: a 403 that names the
/// forwarder, without `WWW-Authenticate`, never a 401 (git erases the stored credential on
/// one). A git client is told in band. Not forwarding, a dead credential is still a 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_misconfigured_forwarder_is_a_403_naming_it_never_a_401() -> TestResult {
    let server = Server::start_with_tweak(|c| {
        c.server.auth.mode = walgit_config::AuthMode::Token;
        c.server.auth.anonymous_read = false;
        c.server.auth.tokens = vec![
            walgit_config::StaticToken {
                principal: "gw".into(),
                token: "gw-token".into(),
                token_env: None,
                write: true,
                admin: true,
            },
            walgit_config::StaticToken {
                principal: "robot".into(),
                token: "robot-token".into(),
                token_env: None,
                write: true,
                admin: false,
            },
        ];
        c.server.auth.trusted_forwarders = vec!["gw".into()];
    })
    .await?;
    let gw = [
        ("Authorization", "Bearer gw-token"),
        ("X-Walgit-Principal", "dev@example.com"),
        ("X-Walgit-Access", "write"),
    ];
    let (st, text, _) = req(&server, reqwest::Method::PUT, "/acme/app/api", &gw).await?;
    assert_eq!(st, 201, "{text}");

    let bad: [&[(&str, &str)]; 4] = [
        // a stale forwarder token
        &[
            ("Authorization", "Bearer stale"),
            ("X-Walgit-Principal", "dev@example.com"),
            ("X-Walgit-Access", "read"),
        ],
        // no forwarder token at all
        &[
            ("X-Walgit-Principal", "dev@example.com"),
            ("X-Walgit-Access", "read"),
        ],
        // a valid token that is not a trusted forwarder, narrowing
        &[
            ("Authorization", "Bearer robot-token"),
            ("X-Walgit-Principal", "dev@example.com"),
            ("X-Walgit-Access", "read"),
        ],
        // a trusted forwarder sending an unknown level
        &[
            ("Authorization", "Bearer gw-token"),
            ("X-Walgit-Principal", "dev@example.com"),
            ("X-Walgit-Access", "owner"),
        ],
    ];
    for headers in bad {
        for (method, path) in [
            (reqwest::Method::GET, "/api/v1/owners"),
            (reqwest::Method::GET, "/acme/app/api"),
            (
                reqwest::Method::GET,
                "/acme/app.git/info/refs?service=git-upload-pack",
            ),
            (reqwest::Method::GET, "/_auth/check"),
        ] {
            let (st, text, h) = req(&server, method.clone(), path, headers).await?;
            assert_ne!(st, 401, "{headers:?} {method} {path}: {text}");
            assert_eq!(st, 403, "{headers:?} {method} {path}: {text}");
            assert!(
                text.contains("forwarder"),
                "{headers:?} {method} {path}: {text}"
            );
            assert!(
                h.get("www-authenticate").is_none(),
                "{headers:?} {method} {path}"
            );
        }
        // LFS (the batch body is parsed first, so send a valid one).
        let (st, text) = req_body(
            &server,
            reqwest::Method::POST,
            "/acme/app.git/info/lfs/objects/batch",
            headers,
            r#"{"operation":"download","objects":[]}"#,
        )
        .await?;
        assert_eq!(
            (st.as_u16(), text.contains("forwarder")),
            (403, true),
            "{headers:?} lfs: {text}"
        );
        for service in ["git-upload-pack", "git-receive-pack"] {
            let mut h = headers.to_vec();
            h.push(("User-Agent", "git/2.46.0"));
            let (st, text, _) = req(
                &server,
                reqwest::Method::GET,
                &format!("/acme/app.git/info/refs?service={service}"),
                &h,
            )
            .await?;
            assert_ne!(st, 401, "{headers:?} {service}: {text}");
            assert!(
                text.contains("ERR walgit: forbidden") && text.contains("forwarder"),
                "{headers:?} {service}: {text}"
            );
        }
    }
    // Not forwarding: a dead credential is still a real 401 (git erases it and asks again).
    let (st, _, h) = req(
        &server,
        reqwest::Method::GET,
        "/acme/app/api",
        &[("Authorization", "Bearer stale")],
    )
    .await?;
    assert_eq!(st, 401);
    assert!(h.get("www-authenticate").is_some());
    Ok(())
}
