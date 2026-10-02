use super::config::{
    deps_current, git_config_bool, git_user_config_paths_at, read_config_for_status,
    upstream_full_ref,
};
use super::discovery::{git_repo_root, git_worktree_info};
use super::identity::FullRefName;
use super::status::git_status_fingerprint;
use super::test_support::{temp_test_dir, write_fake_tracked_repo};
use std::os::unix::ffi::OsStringExt;

fn upstream(root: &std::path::Path) -> (Option<String>, super::config::Dependencies) {
    let info = git_worktree_info(root).expect("repository");
    let mut errors = Vec::new();
    let branch = FullRefName::parse("refs/heads/main")
        .and_then(|full_ref| full_ref.branch_name())
        .expect("test branch");
    let context = read_config_for_status(&info, &branch, &mut errors);
    assert!(errors.is_empty(), "{errors:?}");
    (
        context
            .config
            .as_ref()
            .map(upstream_full_ref)
            .map(|full_ref| full_ref.as_str().to_owned()),
        context.dependencies,
    )
}

#[test]
fn git_config_nosystem_accepts_git_integer_whitespace() {
    let env = shepr_test_support::IsolatedEnv::new();
    env.set(shepr_core::env::EnvVar::GitConfigNoSystem, " \t1");

    let paths = git_user_config_paths_at(std::path::Path::new("/repo")).expect("config paths");
    assert!(!paths.contains(&std::path::PathBuf::from("/etc/gitconfig")));
}

#[test]
fn git_config_bool_uses_git_words_and_scaled_base_zero_integers() {
    assert_eq!(git_config_bool(b"YeS"), Some(true));
    assert_eq!(git_config_bool(b"OFF"), Some(false));
    assert_eq!(git_config_bool(b"0x1K"), Some(true));
    assert_eq!(git_config_bool(b"00"), Some(false));
    assert_eq!(git_config_bool(b"1 "), None);
}

#[test]
fn non_utf8_git_config_nosystem_does_not_refuse_environment() {
    let env = shepr_test_support::IsolatedEnv::new();
    env.set(
        shepr_core::env::EnvVar::GitConfigNoSystem,
        std::ffi::OsString::from_vec(vec![0xff]),
    );

    let paths = git_user_config_paths_at(std::path::Path::new("/repo")).expect("config paths");
    assert!(paths.contains(&std::path::PathBuf::from("/etc/gitconfig")));
}

#[test]
fn git_config_owns_quoting_continuations_and_deprecated_subsections() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("git-config-grammar");
    write_fake_tracked_repo(&root);
    std::fs::write(
        root.join(".git/config"),
        concat!(
            "[remote \"origin\"]\nfetch = +refs/heads/*:refs/remotes/origin/*\n",
            "[branch.main]\nremote = o\"rig\"in\nmerge = refs/heads/\\\nmain\n",
        ),
    )
    .expect("config");
    assert_eq!(
        upstream(&root).0.as_deref(),
        Some("refs/remotes/origin/main")
    );
}

#[test]
fn git_config_wildcards_do_not_cross_gitdir_components() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let base = temp_test_dir("git-config-path-wildcard");
    let root = base.join("one/two/repo");
    write_fake_tracked_repo(&root);
    std::fs::write(
        root.join(".git/alternate"),
        "[branch \"main\"]\nremote = .\nmerge = refs/heads/other\n",
    )
    .expect("include");
    std::fs::write(root.join(".git/config"), format!(
        "[remote \"origin\"]\nfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\nremote = origin\nmerge = refs/heads/main\n[includeIf \"gitdir:{}/*/.git\"]\npath = alternate\n", base.display()
    )).expect("config");
    assert_eq!(
        upstream(&root).0.as_deref(),
        Some("refs/remotes/origin/main")
    );
}

#[test]
fn git_config_tracks_included_and_absent_files_and_symlink_targets() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("git-config-deps");
    write_fake_tracked_repo(&root);
    std::fs::write(
        root.join(".git/config"),
        "[include]\npath = tracking\npath = missing\n",
    )
    .expect("config");
    std::fs::write(
        root.join(".git/first"),
        "[branch \"main\"]\nremote = .\nmerge = refs/heads/first\n",
    )
    .expect("include");
    std::os::unix::fs::symlink("first", root.join(".git/tracking")).expect("symlink");
    let (name, deps) = upstream(&root);
    assert_eq!(name.as_deref(), Some("refs/heads/first"));
    assert!(deps_current(&deps));
    std::fs::write(root.join(".git/missing"), "").expect("new include");
    assert!(!deps_current(&deps));
    let (_, deps) = upstream(&root);
    std::fs::write(root.join(".git/second"), "").expect("second include");
    std::fs::remove_file(root.join(".git/tracking")).expect("remove symlink");
    std::os::unix::fs::symlink("second", root.join(".git/tracking")).expect("replace symlink");
    assert!(!deps_current(&deps));
}

#[test]
fn git_config_applies_onbranch_question_mark_condition() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("git-config-onbranch");
    write_fake_tracked_repo(&root);
    std::fs::write(
        root.join(".git/alternate"),
        "[branch \"main\"]\nremote = .\nmerge = refs/heads/other\n",
    )
    .expect("include");
    std::fs::write(
        root.join(".git/config"),
        "[includeIf \"onbranch:ma?n\"]\npath = alternate\n",
    )
    .expect("config");
    assert_eq!(upstream(&root).0.as_deref(), Some("refs/heads/other"));
}

#[test]
fn git_config_malformed_config_is_not_reusable() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("git-config-invalid");
    write_fake_tracked_repo(&root);
    std::fs::write(root.join(".git/config"), "[broken\n").expect("config");
    let info = git_worktree_info(&root).expect("repository");
    let mut errors = Vec::new();
    let branch = FullRefName::parse("refs/heads/main")
        .and_then(|full_ref| full_ref.branch_name())
        .expect("test branch");
    let context = read_config_for_status(&info, &branch, &mut errors);
    assert!(context.config.is_none());
    assert!(!errors.is_empty());
    assert!(!deps_current(&context.dependencies));
}

fn bare_layout(name: &str, config: &str) -> std::path::PathBuf {
    let bare = temp_test_dir(name);
    std::fs::create_dir_all(bare.join("objects")).expect("test precondition");
    std::fs::create_dir_all(bare.join("refs")).expect("test precondition");
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").expect("test precondition");
    std::fs::write(bare.join("config"), config).expect("test precondition");
    bare
}

#[test]
fn included_core_bare_marks_a_bare_layout_as_a_repository_root() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let bare = bare_layout(
        "included-core-bare",
        "[core]\n\tbare = false\n[include]\n\tpath = bare.cfg\n",
    );
    std::fs::write(bare.join("bare.cfg"), "[core]\n\tbare = true\n").expect("test precondition");

    assert_eq!(git_repo_root(&bare.join("refs")), Some(bare));
}

#[test]
fn global_core_bare_applies_unless_the_repository_config_overrides_it() {
    let env = shepr_test_support::IsolatedEnv::new();
    std::fs::write(env.home().join(".gitconfig"), "[core]\n\tbare = true\n")
        .expect("test precondition");

    let bare = bare_layout("global-core-bare", "[core]\n");
    assert_eq!(git_repo_root(&bare.join("refs")), Some(bare));

    let overridden = bare_layout("global-core-bare-overridden", "[core]\n\tbare = false\n");
    assert_eq!(git_repo_root(&overridden.join("refs")), None);
}

#[test]
fn git_status_fingerprint_honors_remote_fetch_refspec() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("custom-fetch-refspec");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/upstream")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/upstream/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/upstream/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/upstream/main");
}

#[test]
fn git_status_fingerprint_reads_included_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("included-config");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
        root.join(".git/config"),
        "[include]\n\tpath = included.cfg\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/included.cfg"),
            "[remote \"included\"]\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "included");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/included/main");
}

#[test]
fn git_status_fingerprint_applies_repeated_includes_in_order() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("repeated-include");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[include]\n\tpath = included.cfg\n[branch \"main\"]\n\tremote = middle\n[include]\n\tpath = included.cfg\n",
        )
        .expect("test precondition");
    std::fs::write(
            root.join(".git/included.cfg"),
            "[remote \"included\"]\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "included");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/included/main");
}

#[test]
fn git_status_fingerprint_reads_matching_include_if_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("include-if-config");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
        root.join(".git/config"),
        format!(
            "[includeIf \"gitdir:{}\"]\n\tpath = included.cfg\n",
            root.join(".git").display()
        ),
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/included.cfg"),
            "[remote \"included\"]\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "included");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/included/main");
}

#[test]
fn git_status_fingerprint_matches_gitdir_include_if_directory_pattern() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let base = temp_test_dir("include-if-dir");
    let root = base.join("work/repo");
    std::fs::create_dir_all(&root).expect("test precondition");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
        root.join(".git/config"),
        format!(
            "[includeIf \"gitdir:{}/\"]\n\tpath = included.cfg\n",
            base.join("work").display()
        ),
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/included.cfg"),
            "[remote \"included\"]\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "included");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/included/main");
}

#[test]
fn git_status_fingerprint_reads_case_insensitive_config_keys() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("case-insensitive-config");
    write_fake_tracked_repo(&root);
    std::fs::write(
            root.join(".git/config"),
            "[Remote \"origin\"] # remote section\n\tFetch = +refs/heads/*:refs/remotes/origin/*\n[Branch \"main\"] ; branch section\n\tRemote = origin\n\tMerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "origin");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/origin/main");
}

#[test]
fn git_status_fingerprint_keeps_refspecs_for_later_remote_override() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("worktree-remote-override");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/fork")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/fork/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            // Git honours worktreeConfig only in a version 1 repository.
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tworktreeConfig = true\n[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
    std::fs::write(
        root.join(".git/config.worktree"),
        "[branch \"main\"]\n\tremote = fork\n",
    )
    .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "fork");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/fork/main");
}

#[test]
fn git_status_fingerprint_reads_onbranch_include_if_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("include-if-onbranch");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
        root.join(".git/config"),
        "[includeIf \"onbranch:main\"]\n\tpath = included.cfg\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/included.cfg"),
            "[remote \"included\"]\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "included");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/included/main");
}

#[test]
fn git_status_fingerprint_reads_hasconfig_include_if_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("include-if-hasconfig");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[remote \"fork\"]\n\turl = https://example.test/fork.git\n[includeIf \"hasconfig:remote.*.url:**/fork.git\"]\n\tpath = included.cfg\n",
        )
        .expect("test precondition");
    std::fs::write(
            root.join(".git/included.cfg"),
            "[remote \"included\"]\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "included");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/included/main");
}

#[test]
fn git_status_fingerprint_reads_linked_worktree_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let base = temp_test_dir("linked-worktree-config");
    let common_dir = base.join("repo/.git");
    let worktree = base.join("linked");
    let git_dir = common_dir.join("worktrees/linked");
    std::fs::create_dir_all(common_dir.join("objects")).expect("test precondition");
    std::fs::create_dir_all(common_dir.join("refs/heads")).expect("test precondition");
    std::fs::create_dir_all(common_dir.join("refs/remotes/fork")).expect("test precondition");
    std::fs::create_dir_all(&git_dir).expect("test precondition");
    std::fs::create_dir_all(&worktree).expect("test precondition");
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .expect("test precondition");
    std::fs::write(git_dir.join("commondir"), "../..\n").expect("test precondition");
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("test precondition");
    std::fs::write(
        common_dir.join("refs/heads/main"),
        "1111111111111111111111111111111111111111\n",
    )
    .expect("test precondition");
    std::fs::write(
        common_dir.join("refs/remotes/fork/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
            common_dir.join("config"),
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tworktreeConfig = TRUE\n[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
    std::fs::write(
        git_dir.join("config.worktree"),
        "[branch \"main\"]\n\tremote = fork\n",
    )
    .expect("test precondition");

    let fingerprint = git_status_fingerprint(&worktree).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "fork");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/fork/main");
}

#[test]
fn git_status_fingerprint_ignores_inline_fetch_refspec_comment() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("commented-fetch-refspec");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/upstream")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/upstream/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/upstream/* # custom map\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/upstream/main");
    assert_eq!(
        upstream.oid.as_ref().map(super::identity::Oid::as_str),
        Some("2222222222222222222222222222222222222222")
    );
}

#[test]
fn git_status_fingerprint_clears_upstream_for_unmapped_refspec() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("unmapped-fetch-refspec");
    write_fake_tracked_repo(&root);
    std::fs::write(
            root.join(".git/config"),
            "[remote \"origin\"]\n\tfetch = +refs/pull/*:refs/remotes/origin/pr/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    assert_eq!(fingerprint.upstream, None);
}

#[test]
fn git_status_fingerprint_honors_negative_fetch_refspec() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("negative-fetch-refspec");
    write_fake_tracked_repo(&root);
    std::fs::write(
            root.join(".git/config"),
            "[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\tfetch = ^refs/heads/main\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.full_ref.as_str(), "refs/remotes/origin/main");
    assert_eq!(
        upstream.oid.as_ref().map(super::identity::Oid::as_str),
        Some("2222222222222222222222222222222222222222")
    );
}
