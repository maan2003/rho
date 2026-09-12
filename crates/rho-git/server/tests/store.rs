mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{git, push_commit, setup_remote};
use rho_git_proto::{Request, Response};
use rho_git_server::{MirrorStore, Refresh};

fn store(root: &std::path::Path, debounce: Duration) -> Arc<MirrorStore> {
    MirrorStore::new(root, "git", Vec::new(), Refresh { debounce })
}

#[tokio::test]
async fn ensure_inits_a_complete_mirror() {
    let temp = tempfile::tempdir().unwrap();
    let (_source, remote) = setup_remote(temp.path());
    let url = remote.to_str().unwrap();
    let store = store(&temp.path().join("stores"), Duration::ZERO);

    let mirror = store.ensure(url).await.unwrap();
    assert_eq!(mirror, store.mirror_dir(url));
    assert!(mirror.join("HEAD").is_file());
    assert_eq!(
        std::fs::read_to_string(store.store_dir(url).join("url"))
            .unwrap()
            .trim(),
        url
    );
    let refs = git(&mirror, &["for-each-ref", "--format=%(refname)"]);
    assert!(refs.contains("refs/heads/main"), "{refs}");
    assert!(!refs.contains("refs/remotes/"), "{refs}");
    assert_eq!(
        git(&mirror, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert_eq!(git(&mirror, &["config", "gc.pruneExpire"]).trim(), "never");
    assert_eq!(git(&mirror, &["config", "gc.auto"]).trim(), "0");
    assert!(!store.store_dir(url).with_extension("staging").exists());
    assert_eq!(
        store.list().unwrap(),
        vec![(store.store_dir(url), url.to_owned())]
    );
}

#[tokio::test]
async fn ensure_refetches_after_the_debounce_only() {
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let url = remote.to_str().unwrap();
    let store = store(&temp.path().join("stores"), Duration::from_secs(3600));
    let mirror = store.ensure(url).await.unwrap();
    let first = git(&mirror, &["rev-parse", "refs/heads/main"]);

    let second = push_commit(&source, "two\n");
    store.ensure(url).await.unwrap();
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/heads/main"]),
        first,
        "a fresh mirror is served without fetching"
    );

    let eager = self::store(&temp.path().join("stores"), Duration::ZERO);
    eager.ensure(url).await.unwrap();
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/heads/main"]).trim(),
        second
    );
}

#[tokio::test]
async fn a_failed_init_leaves_no_store_behind() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp.path().join("stores"), Duration::ZERO);
    let url = temp.path().join("missing.git");
    let error = store.ensure(url.to_str().unwrap()).await.unwrap_err();
    assert!(format!("{error:#}").contains("fetch"), "{error:#}");
    assert!(store.list().unwrap().is_empty());
    assert!(!store.store_dir(url.to_str().unwrap()).exists());
}

#[tokio::test]
async fn requests_are_answered_over_the_socket() {
    let temp = tempfile::tempdir().unwrap();
    let (_source, remote) = setup_remote(temp.path());
    let url = remote.to_str().unwrap();
    let store = store(&temp.path().join("stores"), Duration::ZERO);

    let reply = store.handle_request("nonsense\n").await;
    assert!(reply.starts_with("error "), "{reply}");
    let reply = store
        .handle_request(&Request::Ensure { url: url.into() }.encode().unwrap())
        .await;
    assert_eq!(
        Response::decode(&reply).unwrap(),
        Response::Ok {
            mirror: store.mirror_dir(url)
        }
    );

    let socket = temp.path().join("store.sock");
    std::fs::write(&socket, "stale").unwrap();
    let listener = MirrorStore::bind(&socket).unwrap();
    let server = tokio::spawn(Arc::clone(&store).serve(listener));
    let socket_for_client = socket.clone();
    let url_for_client = url.to_owned();
    let mirror = tokio::task::spawn_blocking(move || {
        use std::io::{BufRead as _, Write as _};
        let mut stream = std::os::unix::net::UnixStream::connect(socket_for_client).unwrap();
        stream
            .write_all(
                Request::Refresh {
                    url: url_for_client,
                }
                .encode()
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut line)
            .unwrap();
        Response::decode(&line).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        mirror,
        Response::Ok {
            mirror: store.mirror_dir(url)
        }
    );
    server.abort();
}
