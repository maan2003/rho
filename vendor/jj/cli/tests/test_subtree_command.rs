// Copyright 2025 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use jj_lib::secret_backend::SecretBackend;
use testutils::TestResult;

use crate::common::TestEnvironment;

#[test]
fn test_subtree_add_list_and_update() -> TestResult {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");

    work_dir.write_file("lib.txt", "one\n");
    work_dir.run_jj(["bookmark", "create", "source"]).success();
    work_dir
        .run_jj(["bookmark", "create", "old-source"])
        .success();
    work_dir.run_jj(["new", "root()"]).success();
    work_dir.write_file("host.txt", "host\n");

    work_dir
        .run_jj(["subtree", "add", "vendor/lib", "--source", "source"])
        .success();
    assert_eq!(work_dir.read_file("vendor/lib/lib.txt"), "one\n");
    assert!(work_dir.root().join("vendor/lib/.jjsubtree.toml").is_file());

    let output = work_dir.run_jj(["subtree", "list"]).success();
    assert!(output.stdout.raw().starts_with("vendor/lib "));

    work_dir.write_file("vendor/lib/local.txt", "local\n");
    work_dir.run_jj(["bookmark", "create", "host"]).success();
    work_dir.run_jj(["new", "source"]).success();
    work_dir.write_file("lib.txt", "two\n");
    work_dir
        .run_jj(["bookmark", "set", "source", "-r@"])
        .success();
    work_dir.run_jj(["edit", "host"]).success();

    work_dir
        .run_jj(["subtree", "update", "vendor/lib", "--source", "source"])
        .success();
    assert_eq!(work_dir.read_file("vendor/lib/lib.txt"), "two\n");
    assert_eq!(work_dir.read_file("vendor/lib/local.txt"), "local\n");
    assert_eq!(work_dir.read_file("host.txt"), "host\n");

    let output = work_dir
        .run_jj(["subtree", "update", "vendor/lib", "--source", "old-source"])
        .success();
    assert!(output.stderr.raw().contains("applying the change anyway"));
    assert_eq!(work_dir.read_file("vendor/lib/lib.txt"), "one\n");
    assert_eq!(work_dir.read_file("vendor/lib/local.txt"), "local\n");
    Ok(())
}

#[test]
fn test_subtree_add_rejects_existing_destination() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("source", "source\n");
    work_dir.run_jj(["bookmark", "create", "source"]).success();
    work_dir.run_jj(["new", "root()"]).success();
    work_dir.write_file("vendor/lib/existing", "existing\n");

    let output = work_dir.run_jj(["subtree", "add", "vendor/lib", "--source", "source"]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("already exists"));
}

#[test]
fn test_subtree_add_rejects_non_directory_ancestor_and_reserved_path() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("source", "source\n");
    work_dir.run_jj(["bookmark", "create", "source"]).success();
    work_dir.run_jj(["new", "root()"]).success();
    work_dir.write_file("vendor", "must survive\n");

    for path in [".jj/vendor", ".git/vendor"] {
        let output = work_dir.run_jj(["subtree", "add", path, "--source", "source"]);
        assert!(!output.status.success());
        assert!(output.stderr.raw().contains("reserved component"));
        work_dir.run_jj(["status"]).success();
    }

    let output = work_dir.run_jj(["subtree", "add", "vendor/lib", "--source", "source"]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("non-directory ancestor"));
    assert_eq!(work_dir.read_file("vendor"), "must survive\n");
}

#[test]
fn test_subtree_add_preflights_destination_backend_access() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "public contents\n");
    work_dir.run_jj(["bookmark", "create", "source"]).success();
    work_dir.run_jj(["new", "root()"]).success();
    work_dir.write_file("secret-host", "unrelated inaccessible file\n");
    work_dir.run_jj(["debug", "snapshot"]).success();
    SecretBackend::adopt_git_repo(work_dir.root());

    let output = work_dir.run_jj(["subtree", "add", "secret-vendor", "--source", "source"]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("Access denied"));
    assert!(!work_dir.root().join("secret-vendor").exists());

    work_dir
        .run_jj(["subtree", "add", "vendor", "--source", "source"])
        .success();
    assert_eq!(work_dir.read_file("vendor/file"), "public contents\n");
    work_dir.run_jj(["status"]).success();
}

#[test]
fn test_subtree_update_rejects_reserved_metadata_from_source() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "base\n");
    work_dir.run_jj(["bookmark", "create", "source"]).success();
    work_dir.run_jj(["new", "root()"]).success();
    work_dir
        .run_jj(["subtree", "add", "vendor/lib", "--source", "source"])
        .success();
    work_dir.run_jj(["bookmark", "create", "host"]).success();

    work_dir.run_jj(["new", "source"]).success();
    work_dir.write_file("nested/.jjsubtree.toml", "not management metadata\n");
    work_dir
        .run_jj(["bookmark", "set", "source", "-r@"])
        .success();
    work_dir.run_jj(["edit", "host"]).success();

    let output = work_dir.run_jj(["subtree", "update", "vendor/lib", "--source", "source"]);
    assert!(!output.status.success());
    assert!(output.stderr.raw().contains("contains reserved file"));
    assert!(!work_dir.root().join("vendor/lib/nested").exists());
}

#[test]
fn test_subtree_update_conflict() {
    let test_env = TestEnvironment::default();
    test_env.run_jj_in(".", ["git", "init", "repo"]).success();
    let work_dir = test_env.work_dir("repo");
    work_dir.write_file("file", "base\n");
    work_dir.run_jj(["bookmark", "create", "source"]).success();
    work_dir.run_jj(["new", "root()"]).success();
    work_dir
        .run_jj(["subtree", "add", "vendor/lib", "--source", "source"])
        .success();
    work_dir.write_file("vendor/lib/file", "local\n");
    work_dir.run_jj(["bookmark", "create", "host"]).success();

    work_dir.run_jj(["new", "source"]).success();
    work_dir.write_file("file", "upstream\n");
    work_dir
        .run_jj(["bookmark", "set", "source", "-r@"])
        .success();
    work_dir.run_jj(["edit", "host"]).success();
    work_dir
        .run_jj(["subtree", "update", "vendor/lib", "--source", "source"])
        .success();

    let output = work_dir.run_jj(["status"]);
    assert!(output.stdout.raw().contains("vendor/lib/file"));
    assert!(output.stdout.raw().contains("conflict"));
    let conflict_bytes = work_dir.read_file("vendor/lib/file");
    let conflict = std::str::from_utf8(&conflict_bytes).unwrap();
    assert!(conflict.contains("previous source"));
    assert!(conflict.contains("local repository"));
    assert!(conflict.contains("new source"));

    work_dir
        .run_jj(["bookmark", "create", "conflicted-host"])
        .success();
    work_dir.run_jj(["new", "source"]).success();
    work_dir.write_file("next", "next upstream change\n");
    work_dir
        .run_jj(["bookmark", "set", "source", "-r@"])
        .success();
    work_dir.run_jj(["edit", "conflicted-host"]).success();
    work_dir
        .run_jj(["subtree", "update", "vendor/lib", "--source", "source"])
        .success();

    assert_eq!(
        work_dir.read_file("vendor/lib/next"),
        "next upstream change\n"
    );
    let output = work_dir.run_jj(["status"]);
    assert!(output.stdout.raw().contains("vendor/lib/file"));
    assert!(output.stdout.raw().contains("conflict"));
}
