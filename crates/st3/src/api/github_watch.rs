//! `st gh`: a seat's watches on GitHub issues and pull requests.

use super::*;
use crate::github_watch::ThreadRef;

/// Watch one thread as one seat, until an optional deadline.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WatchRequest {
    pub actor: String,
    pub thread: String,
    /// A duration such as `2h`, or an RFC 3339 time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// End one seat's watch. A person names the seat; a seat ends only its own.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UnwatchRequest {
    pub actor: String,
    pub thread: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Post a comment, or a pull request review, as one seat.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommentRequest {
    pub actor: String,
    pub thread: String,
    pub body: String,
    /// `approve`, `request-changes` or `comment` posts a pull request review instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// Watch the thread too, unless the seat already does.
    #[serde(default = "watch_by_default")]
    pub watch: bool,
}

fn watch_by_default() -> bool {
    true
}

/// Record a comment or review a seat posted some other way, by its URL.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OwnRequest {
    pub actor: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct WatchesQuery {
    #[serde(default)]
    agent: Option<String>,
}

fn bad(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::bad(St3Error::new(code, message.into()))
}

/// A deadline from now: a duration such as `90m` or `2h`, or an RFC 3339 time.
fn deadline(until: Option<&str>) -> Result<Option<u128>, ApiError> {
    let Some(until) = until.map(str::trim).filter(|until| !until.is_empty()) else {
        return Ok(None);
    };
    if let Some(at) = crate::github_watch::parse_rfc3339_ms(until) {
        return Ok(Some(at));
    }
    let duration = crate::graph::parse_duration(until, true).map_err(|_| {
        bad(
            "invalid-watch-deadline",
            format!("`{until}` is neither a duration such as 2h nor an RFC 3339 time"),
        )
    })?;
    Ok(Some(
        smallclaims::store::now_ms().saturating_add(u128::from(duration)),
    ))
}

/// What GitHub says of a thread now: its title, link and state. A thread that does not exist,
/// or that this daemon's token cannot read, cannot be watched.
async fn read_thread(thread: &ThreadRef) -> Result<Value, ApiError> {
    let token = crate::resource::github_token().await.map_err(|_| {
        bad(
            "github-unauthenticated",
            crate::resource::GITHUB_AUTH_REMEDY,
        )
    })?;
    read_thread_at(&crate::resource::github_api_base(), &token, thread).await
}

async fn read_thread_at(
    api_base: &str,
    token: &str,
    thread: &ThreadRef,
) -> Result<Value, ApiError> {
    let response = crate::resource::github_api_client()
        .get(format!(
            "{api_base}/repos/{}/issues/{}",
            thread.locator(),
            thread.number
        ))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| {
            bad(
                "github-unreachable",
                format!("GitHub did not answer: {error}"),
            )
        })?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(bad(
            "github-thread-not-found",
            format!("GitHub has no {thread} that this daemon's token can read"),
        ));
    }
    if !status.is_success() {
        return Err(bad(
            "github-unreachable",
            format!("GitHub answered HTTP {status} for {thread}"),
        ));
    }
    response.json::<Value>().await.map_err(|error| {
        bad(
            "github-unreachable",
            format!("GitHub's answer was not JSON: {error}"),
        )
    })
}

/// Only an open thread can be watched: a closed one would never wake the seat.
fn watchable(thread: &ThreadRef, github: &Value) -> Result<(), ApiError> {
    if github.get("state").and_then(Value::as_str) == Some("open") {
        return Ok(());
    }
    Err(bad(
        "github-thread-closed",
        format!("{thread} is already closed, so nothing would wake the seat"),
    ))
}

pub(super) async fn watch(
    State(state): State<AppState>,
    Json(request): Json<WatchRequest>,
) -> Result<Json<Value>, ApiError> {
    let thread = ThreadRef::parse(&request.thread).map_err(ApiError::bad)?;
    if !request.actor.starts_with("agent/") {
        return Err(bad(
            "watch-needs-a-seat",
            "a watch wakes one agent seat; run `st gh watch` inside the seat",
        ));
    }
    let until = deadline(request.until.as_deref())?;
    let github = read_thread(&thread).await?;
    watchable(&thread, &github)?;
    let store = state.store.clone();
    let actor = request.actor.clone();
    let view = blocking_action(move || {
        if !store.seat_live(&actor)? {
            return Err(St3Error::new(
                "watch-needs-a-seat",
                format!("`{actor}` is not a running seat"),
            ));
        }
        store.declare_watch(&thread, &actor, until)
    })
    .await?;
    signal_changed(&state);
    let mut view = view;
    for (name, value) in [
        ("title", github.get("title")),
        ("url", github.get("html_url")),
        ("thread_state", github.get("state")),
    ] {
        if view.get(name).is_none_or(Value::is_null)
            && let Some(value) = value
        {
            view[name] = value.clone();
        }
    }
    Ok(Json(view))
}

/// A comment or review GitHub answered with: its kind, ID, link and author.
#[derive(Debug)]
struct Posted {
    kind: &'static str,
    id: u64,
    url: String,
    login: String,
}

fn posted(kind: &'static str, answer: &Value) -> Result<Posted, ApiError> {
    Ok(Posted {
        kind,
        id: answer
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("github-unreachable", "GitHub's answer named no ID"))?,
        url: answer
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        login: answer
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

async fn github_send(
    request: reqwest::RequestBuilder,
    token: &str,
    what: &str,
) -> Result<Value, ApiError> {
    let response = request
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| {
            bad(
                "github-unreachable",
                format!("GitHub did not answer: {error}"),
            )
        })?;
    let status = response.status();
    let answer = response.json::<Value>().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(answer);
    }
    let message = answer
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Err(bad(
        if status == reqwest::StatusCode::NOT_FOUND {
            "github-thread-not-found"
        } else {
            "github-refused"
        },
        format!("GitHub refused {what} with HTTP {status}: {message}"),
    ))
}

/// Post a comment, or a review of a pull request, and return what GitHub created.
async fn post_at(
    api_base: &str,
    token: &str,
    thread: &ThreadRef,
    body: &str,
    review: Option<&str>,
) -> Result<Posted, ApiError> {
    let client = crate::resource::github_api_client();
    let base = format!("{api_base}/repos/{}", thread.locator());
    match review {
        None => {
            let answer = github_send(
                client
                    .post(format!("{base}/issues/{}/comments", thread.number))
                    .json(&json!({"body": body})),
                token,
                "the comment",
            )
            .await?;
            posted("comment", &answer)
        }
        Some(review) => {
            let event = match review {
                "approve" => "APPROVE",
                "request-changes" => "REQUEST_CHANGES",
                "comment" => "COMMENT",
                other => {
                    return Err(bad(
                        "invalid-github-review",
                        format!("a review is approve, request-changes or comment, not `{other}`"),
                    ));
                }
            };
            let answer = github_send(
                client
                    .post(format!("{base}/pulls/{}/reviews", thread.number))
                    .json(&json!({"body": body, "event": event})),
                token,
                "the review",
            )
            .await?;
            posted("review", &answer)
        }
    }
}

/// The comment or review a GitHub URL names: `#issuecomment-ID` or `#pullrequestreview-ID`.
fn object_of(url: &str) -> Result<(ThreadRef, &'static str, u64), ApiError> {
    let thread = ThreadRef::parse(url).map_err(ApiError::bad)?;
    let fragment = url
        .rsplit_once('#')
        .map(|(_, fragment)| fragment)
        .unwrap_or_default();
    let (kind, id) = if let Some(id) = fragment.strip_prefix("issuecomment-") {
        ("comment", id)
    } else if let Some(id) = fragment.strip_prefix("pullrequestreview-") {
        ("review", id)
    } else {
        return Err(bad(
            "invalid-github-post",
            "the URL names no comment (#issuecomment-ID) or review (#pullrequestreview-ID)",
        ));
    };
    let id = id
        .parse::<u64>()
        .map_err(|_| bad("invalid-github-post", format!("`{id}` is not a GitHub ID")))?;
    Ok((thread, kind, id))
}

/// Read a comment or review by its ID, and the login this daemon's token posts as.
async fn read_object_at(
    api_base: &str,
    token: &str,
    thread: &ThreadRef,
    kind: &'static str,
    id: u64,
) -> Result<(Posted, String), ApiError> {
    let client = crate::resource::github_api_client();
    let base = format!("{api_base}/repos/{}", thread.locator());
    let url = if kind == "review" {
        format!("{base}/pulls/{}/reviews/{id}", thread.number)
    } else {
        format!("{base}/issues/comments/{id}")
    };
    let object = posted(
        kind,
        &github_send(client.get(url), token, "the read").await?,
    )?;
    let me = github_send(client.get(format!("{api_base}/user")), token, "the read").await?;
    let login = me
        .get("login")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok((object, login))
}

pub(super) async fn comment(
    State(state): State<AppState>,
    Json(request): Json<CommentRequest>,
) -> Result<Json<Value>, ApiError> {
    let thread = ThreadRef::parse(&request.thread).map_err(ApiError::bad)?;
    if !request.actor.starts_with("agent/") {
        return Err(bad(
            "watch-needs-a-seat",
            "st gh comment posts as an agent seat; run it inside the seat",
        ));
    }
    if request.body.trim().is_empty() {
        return Err(bad("empty-github-comment", "the comment has no text"));
    }
    let token = crate::resource::github_token().await.map_err(|_| {
        bad(
            "github-unauthenticated",
            crate::resource::GITHUB_AUTH_REMEDY,
        )
    })?;
    // The seat's wakes on this thread wait until st knows the new comment's ID.
    let in_flight = crate::github_watch::PostInFlight::begin(&request.actor, &thread);
    let posted = post_at(
        &crate::resource::github_api_base(),
        &token,
        &thread,
        &request.body,
        request.review.as_deref(),
    )
    .await?;
    let store = state.store.clone();
    let actor = request.actor.clone();
    let watch = request.watch;
    let recorded_thread = thread.clone();
    let (record, view) = blocking_action(move || {
        let record = store.record_github_post(
            &actor,
            &recorded_thread,
            posted.kind,
            posted.id,
            &posted.url,
            &posted.login,
        )?;
        let view = if watch && store.live_watch(&recorded_thread.watch(&actor))?.is_none() {
            Some(store.declare_watch(&recorded_thread, &actor, None)?)
        } else {
            store.watch_view(&recorded_thread.watch(&actor))?
        };
        Ok((record, view))
    })
    .await?;
    drop(in_flight);
    signal_changed(&state);
    let mut record = record;
    record["watch"] = view.unwrap_or(Value::Null);
    Ok(Json(record))
}

pub(super) async fn own(
    State(state): State<AppState>,
    Json(request): Json<OwnRequest>,
) -> Result<Json<Value>, ApiError> {
    if !request.actor.starts_with("agent/") {
        return Err(bad(
            "watch-needs-a-seat",
            "a comment is recorded as one agent seat's; run st gh own inside the seat",
        ));
    }
    let (thread, kind, id) = object_of(&request.url)?;
    let token = crate::resource::github_token().await.map_err(|_| {
        bad(
            "github-unauthenticated",
            crate::resource::GITHUB_AUTH_REMEDY,
        )
    })?;
    let (object, login) = read_object_at(
        &crate::resource::github_api_base(),
        &token,
        &thread,
        kind,
        id,
    )
    .await?;
    if object.login.is_empty() || !object.login.eq_ignore_ascii_case(&login) {
        return Err(bad(
            "github-post-not-ours",
            format!(
                "{kind} {id} was posted by @{}, not by @{login}, the login this host posts as",
                object.login
            ),
        ));
    }
    let store = state.store.clone();
    let actor = request.actor.clone();
    let record = blocking_action(move || {
        store.record_github_post(&actor, &thread, kind, id, &object.url, &object.login)
    })
    .await?;
    signal_changed(&state);
    Ok(Json(record))
}

pub(super) async fn unwatch(
    State(state): State<AppState>,
    Json(request): Json<UnwatchRequest>,
) -> Result<Json<Value>, ApiError> {
    let thread = ThreadRef::parse(&request.thread).map_err(ApiError::bad)?;
    let agent = if request.actor.starts_with("agent/") {
        if request
            .agent
            .as_deref()
            .is_some_and(|agent| agent != request.actor)
        {
            return Err(bad(
                "foreign-agent-actor",
                "a seat ends only its own watches",
            ));
        }
        request.actor.clone()
    } else {
        request.agent.clone().ok_or_else(|| {
            bad(
                "missing-watch-seat",
                "name the seat whose watch to end with --agent",
            )
        })?
    };
    let subject = thread.watch(&agent);
    let store = state.store.clone();
    let ended = blocking_action(move || store.end_watch(&subject, "unwatched", None)).await?;
    signal_changed(&state);
    Ok(Json(json!({
        "thread": thread.to_string(),
        "agent": agent,
        "subject": thread.watch(&agent),
        "ended": ended,
    })))
}

pub(super) async fn watches(
    State(state): State<AppState>,
    Query(query): Query<WatchesQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let store = state.store.clone();
    let agent = query.agent.filter(|agent| !agent.is_empty());
    blocking_action(move || store.watches(agent.as_deref()))
        .await
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn request(app: Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        let value = if status.is_success() {
            envelope["value"].clone()
        } else {
            envelope
        };
        (status, value)
    }

    /// A GitHub that knows one open and one closed issue.
    async fn fake_github() -> String {
        let app = Router::new()
            .route(
                "/repos/acme/garden/issues/12",
                get(|| async {
                    Json(
                        json!({"number": 12, "state": "open", "title": "Add the seed catalog",
                        "html_url": "https://github.com/acme/garden/pull/12"}),
                    )
                }),
            )
            .route(
                "/repos/acme/garden/issues/14",
                get(|| async { Json(json!({"number": 14, "state": "closed", "title": "Old"})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        base
    }

    /// A GitHub where this token posts as `fleet-login`, with one comment of its own and one of
    /// a person's.
    async fn posting_github() -> String {
        let app = Router::new()
            .route(
                "/repos/acme/garden/issues/12/comments",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["body"], "The seed list is ready.");
                    Json(json!({"id": 501, "user": {"login": "fleet-login"},
                        "html_url": "https://github.com/acme/garden/pull/12#issuecomment-501"}))
                }),
            )
            .route(
                "/repos/acme/garden/pulls/12/reviews",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["event"], "REQUEST_CHANGES");
                    Json(json!({"id": 7001, "user": {"login": "fleet-login"},
                        "html_url": "https://github.com/acme/garden/pull/12#pullrequestreview-7001"}))
                }),
            )
            .route(
                "/repos/acme/garden/issues/comments/502",
                get(|| async { Json(json!({"id": 502, "user": {"login": "nathan-example"}})) }),
            )
            .route(
                "/repos/acme/garden/issues/comments/503",
                get(|| async { Json(json!({"id": 503, "user": {"login": "Fleet-Login"},
                    "html_url": "https://github.com/acme/garden/issues/12#issuecomment-503"})) }),
            )
            .route("/user", get(|| async { Json(json!({"login": "fleet-login"})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        base
    }

    #[tokio::test]
    async fn a_post_returns_its_github_id_and_only_this_logins_posts_can_be_recorded() {
        let github = posting_github().await;
        let thread = ThreadRef::parse("acme/garden#12").unwrap();
        let comment = post_at(
            &github,
            "orchid-token",
            &thread,
            "The seed list is ready.",
            None,
        )
        .await
        .unwrap();
        assert_eq!((comment.kind, comment.id), ("comment", 501));
        assert_eq!(comment.login, "fleet-login");
        let review = post_at(
            &github,
            "orchid-token",
            &thread,
            "Two fixes.",
            Some("request-changes"),
        )
        .await
        .unwrap();
        assert_eq!((review.kind, review.id), ("review", 7001));
        assert_eq!(
            post_at(&github, "orchid-token", &thread, "x", Some("maybe"))
                .await
                .unwrap_err()
                .code,
            "invalid-github-review"
        );

        let (thread, kind, id) =
            object_of("https://github.com/acme/garden/issues/12#issuecomment-502").unwrap();
        let (object, login) = read_object_at(&github, "orchid-token", &thread, kind, id)
            .await
            .unwrap();
        assert_eq!(
            (object.login.as_str(), login.as_str()),
            ("nathan-example", "fleet-login")
        );
        let (thread, kind, id) =
            object_of("https://github.com/acme/garden/issues/12#issuecomment-503").unwrap();
        let (object, login) = read_object_at(&github, "orchid-token", &thread, kind, id)
            .await
            .unwrap();
        assert!(object.login.eq_ignore_ascii_case(&login));
        assert_eq!(
            object_of("https://github.com/acme/garden/pull/12#pullrequestreview-7001")
                .unwrap()
                .1,
            "review"
        );
        assert_eq!(
            object_of("https://github.com/acme/garden/pull/12")
                .unwrap_err()
                .code,
            "invalid-github-post"
        );
    }

    #[tokio::test]
    async fn only_an_open_thread_that_github_shows_can_be_watched() {
        let github = fake_github().await;
        let open = ThreadRef::parse("acme/garden#12").unwrap();
        let answer = read_thread_at(&github, "orchid-token", &open)
            .await
            .unwrap();
        assert!(watchable(&open, &answer).is_ok());
        let closed = ThreadRef::parse("acme/garden#14").unwrap();
        let answer = read_thread_at(&github, "orchid-token", &closed)
            .await
            .unwrap();
        assert_eq!(
            watchable(&closed, &answer).unwrap_err().code,
            "github-thread-closed"
        );
        let missing = ThreadRef::parse("acme/garden#99").unwrap();
        assert_eq!(
            read_thread_at(&github, "orchid-token", &missing)
                .await
                .unwrap_err()
                .code,
            "github-thread-not-found"
        );
    }

    #[tokio::test]
    async fn a_seat_watches_lists_and_ends_its_own_watches_and_a_person_ends_any() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let store = state.store.clone();
        let intent = crate::graph::parse_internal_intent(
            "version 2\nagent \"example.planner\" { workspace \"/tmp\"; command \"true\" }\n",
            "node",
        )
        .unwrap();
        store.apply_internal(&intent, "watch-seat").unwrap();
        let app = router(state);
        let planner = "agent/example.planner";

        // A person or a bad reference is refused before GitHub is asked.
        let (status, refused) = request(
            app.clone(),
            "POST",
            "/v1/github/watch",
            json!({"actor": "person/avery", "thread": "acme/garden#12"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(refused["code"], "watch-needs-a-seat");
        let (_, refused) = request(
            app.clone(),
            "POST",
            "/v1/github/watch",
            json!({"actor": planner, "thread": "acme/garden"}),
        )
        .await;
        assert_eq!(refused["code"], "invalid-github-thread");

        let thread = ThreadRef::parse("acme/garden#12").unwrap();
        store.declare_watch(&thread, planner, None).unwrap();
        let (status, listed) = request(
            app.clone(),
            "GET",
            "/v1/github/watches?agent=agent%2Fexample.planner",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed[0]["thread"], "acme/garden#12");
        assert_eq!(listed[0]["state"], "active");

        // A seat ends only its own watch; a person names the seat.
        let (_, refused) = request(
            app.clone(),
            "POST",
            "/v1/github/unwatch",
            json!({"actor": "agent/example.other", "thread": "acme/garden#12", "agent": planner}),
        )
        .await;
        assert_eq!(refused["code"], "foreign-agent-actor");
        let (_, refused) = request(
            app.clone(),
            "POST",
            "/v1/github/unwatch",
            json!({"actor": "person/avery", "thread": "acme/garden#12"}),
        )
        .await;
        assert_eq!(refused["code"], "missing-watch-seat");
        let (status, ended) = request(
            app.clone(),
            "POST",
            "/v1/github/unwatch",
            json!({"actor": planner, "thread": "https://github.com/acme/garden/pull/12"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ended["ended"], true);
        let (_, listed) = request(app.clone(), "GET", "/v1/github/watches", Value::Null).await;
        assert_eq!(listed[0]["state"], "ended");
        assert_eq!(listed[0]["ended"], "unwatched");
        let (_, again) = request(
            app,
            "POST",
            "/v1/github/unwatch",
            json!({"actor": "person/avery", "thread": "acme/garden#12", "agent": planner}),
        )
        .await;
        assert_eq!(again["ended"], false);
    }
}
