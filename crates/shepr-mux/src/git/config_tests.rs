use super::config::*;
use crate::git::{
    discovery::{git_ref_storage_is_reftable, git_repo_root, git_worktree_info},
    status::git_status_fingerprint,
    test_support::{temp_test_dir, write_fake_tracked_repo},
};

#[test]
fn tilde_git_config_paths_require_a_valid_home() {
    let env = shepr_test_support::IsolatedEnv::new();
    let config = env.path().join("config");
    for home in [None, Some(""), Some("relative/home")] {
        match home {
            Some(home) => env.set("HOME", home),
            None => env.remove("HOME"),
        }
        assert!(normalize_gitdir_include_pattern("~/repo", &config).is_none());
        assert!(resolve_include_path(&config, "~/included.cfg").is_none());
    }
}

/// A directory laid out as a Git directory (`HEAD`, `objects`, `refs`) whose
/// own `config` holds `config`.
fn bare_layout(name: &str, config: &str) -> std::path::PathBuf {
    let bare = temp_test_dir(name);
    std::fs::create_dir_all(bare.join("objects")).expect("test precondition");
    std::fs::create_dir_all(bare.join("refs")).expect("test precondition");
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").expect("test precondition");
    std::fs::write(bare.join("config"), config).expect("test precondition");
    bare
}

/// Git resolves `core.bare` through includes: a bare repository whose
/// config reaches `bare = true` only through `include.path` is bare to
/// `git rev-parse --is-bare-repository`, so it is a repository root here.
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

/// Git resolves `core.bare` through the global config too, with the
/// repository's own config read last and winning.
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

/// Git's system file precedes global config. Its environment overrides select
/// one system file, suppress both default global files when a global override
/// is set, and allow `GIT_CONFIG_NOSYSTEM` to skip the system level.
#[test]
fn git_config_environment_files_follow_git_scope_precedence() {
    let env = shepr_test_support::IsolatedEnv::new();
    env.remove(shepr_core::env::EnvVar::GitConfigNoSystem);
    let system = env.path().join("system.gitconfig");
    let global = env.path().join("global.gitconfig");
    env.set(shepr_core::env::EnvVar::GitConfigSystem, &system);
    env.set(shepr_core::env::EnvVar::GitConfigGlobal, &global);

    let bare = bare_layout("git-config-environment-scopes", "[core]\n");
    std::fs::write(env.home().join(".gitconfig"), "[core]\n\tbare = true\n")
        .expect("test precondition");
    std::fs::write(&system, "[core]\n").expect("test precondition");
    std::fs::write(&global, "[core]\n").expect("test precondition");
    assert_eq!(
        git_repo_root(&bare.join("refs")),
        None,
        "GIT_CONFIG_GLOBAL replaces the default ~/.gitconfig"
    );

    std::fs::write(&system, "[core]\n\tbare = true\n").expect("test precondition");
    std::fs::write(&global, "[core]\n\tbare = false\n").expect("test precondition");
    assert_eq!(
        git_repo_root(&bare.join("refs")),
        None,
        "global config follows and overrides system config"
    );

    std::fs::write(&global, "[core]\n").expect("test precondition");
    env.set(shepr_core::env::EnvVar::GitConfigNoSystem, "yes");
    assert_eq!(
        git_repo_root(&bare.join("refs")),
        None,
        "GIT_CONFIG_NOSYSTEM skips the selected system file"
    );

    env.set(shepr_core::env::EnvVar::GitConfigNoSystem, "false");
    assert_eq!(git_repo_root(&bare.join("refs")), Some(bare.clone()));

    std::fs::write(bare.join("config"), "[core]\n\tbare = false\n").expect("test precondition");
    assert_eq!(git_repo_root(&bare.join("refs")), None);
}

#[test]
fn relative_git_config_overrides_resolve_from_the_repository_root() {
    let env = shepr_test_support::IsolatedEnv::new();
    let bare = bare_layout("relative-git-config-override", "[core]\n");
    std::fs::write(
        bare.join("relative-global.gitconfig"),
        "[core]\n\tbare = true\n",
    )
    .expect("test precondition");
    env.set(
        shepr_core::env::EnvVar::GitConfigGlobal,
        "relative-global.gitconfig",
    );

    assert_eq!(git_repo_root(&bare.join("refs")), Some(bare));
}

#[test]
fn refused_git_config_file_overrides_are_reported_without_falling_back() {
    for var in [
        shepr_core::env::EnvVar::GitConfigSystem,
        shepr_core::env::EnvVar::GitConfigGlobal,
    ] {
        let env = shepr_test_support::IsolatedEnv::new();
        env.remove(shepr_core::env::EnvVar::GitConfigNoSystem);
        let root = temp_test_dir("refused-git-config-override");
        write_fake_tracked_repo(&root);
        let config = root.join(".git/config");
        std::fs::write(
            &config,
            "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
        let info = git_worktree_info(&root).expect("test precondition");
        env.set(var, " leading-space ");

        let mut errors = Vec::new();
        let (branch, config, deps) = read_config_for_status(&info, "main", &mut errors);

        assert_eq!(branch, "main");
        assert!(config.is_none(), "a refused override must not use defaults");
        assert!(
            deps.is_empty(),
            "no config files are consulted after refusal"
        );
        assert!(
            errors.iter().any(|error| matches!(
                error,
                crate::git::GitReadError::ConfigEnvironment { message }
                    if message.contains(var.name())
            )),
            "{errors:?}"
        );
    }
}

#[test]
fn git_command_scope_config_overrides_files_in_pair_order() {
    let env = shepr_test_support::IsolatedEnv::new();
    let bare = bare_layout("git-config-command-scope", "[core]\n\tbare = false\n");
    env.set(shepr_core::env::EnvVar::GitConfigCount, "2");
    env.set("GIT_CONFIG_KEY_0", "CORE.BARE");
    env.set("GIT_CONFIG_VALUE_0", "true");
    env.set("GIT_CONFIG_KEY_1", "core.bare");
    env.set("GIT_CONFIG_VALUE_1", "false");
    assert_eq!(git_repo_root(&bare), None, "the last pair wins");

    env.set(shepr_core::env::EnvVar::GitConfigCount, "1");
    assert_eq!(git_repo_root(&bare), Some(bare));
}

#[test]
fn git_command_scope_config_requires_every_indexed_pair() {
    let env = shepr_test_support::IsolatedEnv::new();
    env.set(shepr_core::env::EnvVar::GitConfigCount, "1");
    env.set("GIT_CONFIG_KEY_0", "core.bare");

    let error = match shepr_core::env::read_git_config_parameters() {
        Ok(_) => panic!("missing config value must refuse"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("GIT_CONFIG_VALUE_0"), "{error}");
}

/// `core.bare` takes Git's boolean grammar, each spelling checked against
/// `git rev-parse --is-bare-repository`. Git refuses to run on a malformed
/// value such as `maybe`; discovery reads it as not bare.
#[test]
fn core_bare_takes_git_boolean_spellings() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let cases = [
        ("\tbare = yes\n", true),
        ("\tbare = on\n", true),
        ("\tbare = 1\n", true),
        ("\tbare = 2\n", true),
        ("\tbare = TRUE\n", true),
        ("\tbare\n", true),
        ("\tbare = true\n\tbare =\n", false),
        ("\tbare = no\n", false),
        ("\tbare = off\n", false),
        ("\tbare = 0\n", false),
        ("\tbare = maybe\n", false),
    ];
    for (index, (lines, bare)) in cases.into_iter().enumerate() {
        let dir = bare_layout(
            &format!("core-bare-spelling-{index}"),
            &format!("[core]\n{lines}"),
        );
        let expected = bare.then(|| dir.clone());
        assert_eq!(git_repo_root(&dir.join("refs")), expected, "{lines:?}");
    }
    assert_eq!(git_config_bool("1k"), Some(true));
    assert_eq!(git_config_bool("0g"), Some(false));
    assert_eq!(git_config_bool("-1"), Some(true));
}

/// Git reads `extensions.refstorage` from the repository's own config file
/// alone: `git rev-parse --show-ref-format` still reports `files` when
/// `reftable` comes only from an include or the global config.
#[test]
fn ref_storage_reads_only_the_repository_config_file() {
    let env = shepr_test_support::IsolatedEnv::new();
    let repo = temp_test_dir("repository-only-refstorage");
    write_fake_tracked_repo(&repo);
    let config = repo.join(".git/config");
    std::fs::write(
        &config,
        "[core]\n\trepositoryformatversion = 1\n[include]\n\tpath = extensions.cfg\n",
    )
    .expect("test precondition");
    std::fs::write(
        repo.join(".git/extensions.cfg"),
        "[extensions]\n\trefstorage = reftable\n",
    )
    .expect("test precondition");
    std::fs::write(
        env.home().join(".gitconfig"),
        "[extensions]\n\trefstorage = reftable\n",
    )
    .expect("test precondition");
    let info = git_worktree_info(&repo).expect("test precondition");

    let (is_reftable, _) = git_ref_storage_is_reftable(&info).expect("test precondition");
    assert!(!is_reftable);

    std::fs::write(
        &config,
        "[core]\n\trepositoryformatversion = 1\n[Extensions]\n\tRefStorage = reftable\n",
    )
    .expect("test precondition");
    let (is_reftable, deps) = git_ref_storage_is_reftable(&info).expect("test precondition");
    assert!(is_reftable);
    assert!(deps.iter().any(|dep| dep.0 == config));
    assert!(deps_current(&deps));
    std::fs::write(&config, "[core]\n\trepositoryformatversion = 1\n").expect("test precondition");
    assert!(!deps_current(&deps));
}

#[test]
fn git_config_value_distinguishes_missing_key_from_read_failure() {
    use std::os::unix::fs::symlink;

    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("git-config-read-failure");
    write_fake_tracked_repo(&root);
    let info = git_worktree_info(&root).expect("test precondition");
    let config = root.join(".git/config");

    std::fs::write(&config, "[core]\n\tbare = false\n").expect("test precondition");
    let (missing, _) = read_repository_format_value(&config, "extensions", "refstorage")
        .expect("an absent key is not a read failure");
    assert_eq!(missing, None);
    let (absent_file, _) = read_repository_format_value(
        &root.join(".git/no-such-config"),
        "extensions",
        "refstorage",
    )
    .expect("an absent file is not a read failure");
    assert_eq!(absent_file, None);

    std::fs::remove_file(&config).expect("test precondition");
    symlink("config", &config).expect("test precondition");
    assert!(read_repository_format_value(&config, "extensions", "refstorage").is_err());
    assert!(git_ref_storage_is_reftable(&info).is_err());
    assert!(
        read_config_value(&info, "main", std::slice::from_ref(&config), "core", "bare").is_err()
    );
    // Discovery does not read the config. The status read does, and reports
    // the unreadable file instead of treating the repository as absent.
    let info = git_worktree_info(&root).expect("discovery does not depend on the config");
    let mut errors = Vec::new();
    let _branch_config = read_config_for_status(&info, "main", &mut errors);
    assert!(
        errors.iter().any(|error| matches!(
            error,
            crate::git::GitReadError::FileRead { path, .. } if path == &config
        )),
        "{errors:?}"
    );
}

#[test]
fn config_symlink_retarget_invalidates_context() {
    use std::os::unix::fs::symlink;

    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("config-symlink-retarget");
    write_fake_tracked_repo(&root);
    let alias = root.join("branch.cfg");
    let first = root.join("first.cfg");
    let second = root.join("second.cfg");
    std::fs::write(&first, "").expect("test precondition");
    std::fs::write(&second, "").expect("test precondition");
    symlink(&first, &alias).expect("test precondition");
    let context = read_config_with_user_paths(
        &git_worktree_info(&root).expect("test precondition"),
        "main",
        vec![alias.clone()],
    );
    std::fs::remove_file(&alias).expect("test precondition");
    symlink(&second, &alias).expect("test precondition");

    assert!(!deps_current(&context.2));
}

#[test]
fn config_read_error_retries_next_refresh() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("config-read-error");
    write_fake_tracked_repo(&root);
    let info = git_worktree_info(&root).expect("test precondition");
    std::fs::write(root.join(".git/config"), [0xff]).expect("test precondition");
    let context = read_config(&info, "main");
    assert!(!deps_current(&context.2));
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
    assert_eq!(upstream.full_ref, "refs/remotes/upstream/main");
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
    assert_eq!(upstream.full_ref, "refs/remotes/included/main");
}

#[test]
fn git_status_branch_config_reads_user_config_before_repo_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("user-config");
    write_fake_tracked_repo(&root);
    let user_config = root.join("user.gitconfig");
    std::fs::write(root.join(".git/config"), "").expect("test precondition");
    std::fs::write(
            &user_config,
            "[remote \"global\"]\n\tfetch = +refs/heads/*:refs/remotes/global/*\n[branch \"main\"]\n\tremote = global\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let info = git_worktree_info(&root).expect("test precondition");
    let config = read_config_with_user_paths(&info, "main", vec![user_config])
        .1
        .expect("test precondition");

    assert_eq!(config.remote, "global");
    assert_eq!(
        upstream_full_ref(&config).as_deref(),
        Some("refs/remotes/global/main")
    );
}

#[test]
fn git_status_branch_config_repo_config_overrides_user_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("repo-overrides-user-config");
    write_fake_tracked_repo(&root);
    let user_config = root.join("user.gitconfig");
    std::fs::write(
            &user_config,
            "[remote \"global\"]\n\tfetch = +refs/heads/*:refs/remotes/global/*\n[branch \"main\"]\n\tremote = global\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let info = git_worktree_info(&root).expect("test precondition");
    let config = read_config_with_user_paths(&info, "main", vec![user_config])
        .1
        .expect("test precondition");

    assert_eq!(config.remote, "origin");
    assert_eq!(
        upstream_full_ref(&config).as_deref(),
        Some("refs/remotes/origin/main")
    );
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
    assert_eq!(upstream.full_ref, "refs/remotes/included/main");
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
    assert_eq!(upstream.full_ref, "refs/remotes/included/main");
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
    assert_eq!(upstream.full_ref, "refs/remotes/included/main");
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
    assert_eq!(upstream.full_ref, "refs/remotes/origin/main");
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
            "[extensions]\n\tworktreeConfig = true\n[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
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
    assert_eq!(upstream.full_ref, "refs/remotes/fork/main");
}

#[test]
fn git_status_fingerprint_ignores_worktree_config_when_extension_disabled() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("worktree-config-disabled");
    write_fake_tracked_repo(&root);
    std::fs::create_dir_all(root.join(".git/refs/remotes/fork")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/fork/main"),
        "3333333333333333333333333333333333333333\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
    std::fs::write(
        root.join(".git/config.worktree"),
        "[branch \"main\"]\n\tremote = fork\n",
    )
    .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "origin");
    assert_eq!(upstream.full_ref, "refs/remotes/origin/main");
}

#[test]
fn git_status_fingerprint_accepts_git_boolean_worktree_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("worktree-config-boolean");
    write_fake_tracked_repo(&root);
    std::fs::create_dir_all(root.join(".git/refs/remotes/fork")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/fork/main"),
        "3333333333333333333333333333333333333333\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[extensions]\n\tworktreeConfig\n[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
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
    assert_eq!(upstream.full_ref, "refs/remotes/fork/main");
}

#[test]
fn git_status_fingerprint_uses_last_worktree_config_boolean() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("worktree-config-duplicate-boolean");
    write_fake_tracked_repo(&root);
    std::fs::create_dir_all(root.join(".git/refs/remotes/fork")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/fork/main"),
        "3333333333333333333333333333333333333333\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[extensions]\n\tworktreeConfig = false\n\tworktreeConfig = true\n[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
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
    assert_eq!(upstream.full_ref, "refs/remotes/fork/main");
}

#[test]
fn git_status_fingerprint_ignores_included_worktree_config_extension() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("worktree-config-included-extension");
    write_fake_tracked_repo(&root);
    std::fs::create_dir_all(root.join(".git/refs/remotes/fork")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/fork/main"),
        "3333333333333333333333333333333333333333\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[include]\n\tpath = extension.cfg\n[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
    std::fs::write(
        root.join(".git/extension.cfg"),
        "[extensions]\n\tworktreeConfig = true\n",
    )
    .expect("test precondition");
    std::fs::write(
        root.join(".git/config.worktree"),
        "[branch \"main\"]\n\tremote = fork\n",
    )
    .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "origin");
    assert_eq!(upstream.full_ref, "refs/remotes/origin/main");
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
    assert_eq!(upstream.full_ref, "refs/remotes/included/main");
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
            "[remote \"fork\"]\n\turl = https://example.test/fork.git\n[includeIf \"hasconfig:remote.*.url:*fork.git\"]\n\tpath = included.cfg\n",
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
    assert_eq!(upstream.full_ref, "refs/remotes/included/main");
}

#[test]
fn git_status_fingerprint_matches_user_hasconfig_against_repo_remote_url() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("include-if-hasconfig-user-repo-url");
    let user_config = root.join("user.gitconfig");
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
        "[remote \"fork\"]\n\turl = https://example.test/fork.git\n",
    )
    .expect("test precondition");
    std::fs::write(
        &user_config,
        "[includeIf \"hasconfig:remote.*.url:*fork.git\"]\n\tpath = user-included.cfg\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join("user-included.cfg"),
            "[remote \"included\"]\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let info = git_worktree_info(&root).expect("test precondition");
    let config = read_config_with_user_paths(&info, "main", vec![user_config])
        .1
        .expect("test precondition");

    assert_eq!(config.remote, "included");
    assert_eq!(config.merge_ref, "refs/heads/main");
}

#[test]
fn git_status_fingerprint_skips_hasconfig_include_that_defines_remote_url() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("include-if-hasconfig-rejects-remote-url");
    let user_config = root.join("user.gitconfig");
    write_fake_tracked_repo(&root);
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "3333333333333333333333333333333333333333\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[remote \"fork\"]\n\turl = https://example.test/fork.git\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
    std::fs::write(
        &user_config,
        "[includeIf \"hasconfig:remote.*.url:*fork.git\"]\n\tpath = user-included.cfg\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join("user-included.cfg"),
            "[remote \"included\"]\n\turl = https://example.test/included.git\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let info = git_worktree_info(&root).expect("test precondition");
    let config = read_config_with_user_paths(&info, "main", vec![user_config])
        .1
        .expect("test precondition");

    assert_eq!(config.remote, "origin");
    assert_eq!(config.merge_ref, "refs/heads/main");
}

#[test]
fn git_status_fingerprint_skips_hasconfig_include_chain_that_defines_remote_url() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("include-if-hasconfig-rejects-nested-remote-url");
    let user_config = root.join("user.gitconfig");
    write_fake_tracked_repo(&root);
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "3333333333333333333333333333333333333333\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join(".git/config"),
            "[remote \"fork\"]\n\turl = https://example.test/fork.git\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
    std::fs::write(
        &user_config,
        "[includeIf \"hasconfig:remote.*.url:*fork.git\"]\n\tpath = user-included.cfg\n",
    )
    .expect("test precondition");
    std::fs::write(
            root.join("user-included.cfg"),
            "[include]\n\tpath = nested-remote.cfg\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
    std::fs::write(
            root.join("nested-remote.cfg"),
            "[remote \"included\"]\n\turl = https://example.test/included.git\n\tfetch = +refs/heads/*:refs/remotes/included/*\n",
        )
        .expect("test precondition");

    let info = git_worktree_info(&root).expect("test precondition");
    let config = read_config_with_user_paths(&info, "main", vec![user_config])
        .1
        .expect("test precondition");

    assert_eq!(config.remote, "origin");
    assert_eq!(config.merge_ref, "refs/heads/main");
}

#[test]
fn git_status_fingerprint_ignores_worktree_urls_for_hasconfig() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("include-if-hasconfig-worktree-url");
    write_fake_tracked_repo(&root);
    std::fs::write(
            root.join(".git/config"),
            "[extensions]\n\tworktreeConfig = true\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");
    std::fs::write(
            root.join(".git/config.worktree"),
            "[remote \"fork\"]\n\turl = https://example.test/fork.git\n[includeIf \"hasconfig:remote.*.url:*fork.git\"]\n\tpath = included.cfg\n",
        )
        .expect("test precondition");
    std::fs::write(
        root.join(".git/included.cfg"),
        "[branch \"main\"]\n\tremote = included\n",
    )
    .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "origin");
    assert_eq!(upstream.full_ref, "refs/remotes/origin/main");
}

#[test]
fn git_status_fingerprint_stops_recursive_include_cycles() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let root = temp_test_dir("include-cycle");
    write_fake_tracked_repo(&root);
    std::fs::remove_dir_all(root.join(".git/refs/remotes/origin")).expect("test precondition");
    std::fs::create_dir_all(root.join(".git/refs/remotes/included")).expect("test precondition");
    std::fs::write(
        root.join(".git/refs/remotes/included/main"),
        "2222222222222222222222222222222222222222\n",
    )
    .expect("test precondition");
    std::fs::write(root.join(".git/config"), "[include]\n\tpath = a.cfg\n")
        .expect("test precondition");
    std::fs::write(root.join(".git/a.cfg"), "[include]\n\tpath = b.cfg\n")
        .expect("test precondition");
    std::fs::write(
            root.join(".git/b.cfg"),
            "[include]\n\tpath = a.cfg\n[remote \"included\"]\n\tfetch = +refs/heads/*:refs/remotes/included/*\n[branch \"main\"]\n\tremote = included\n\tmerge = refs/heads/main\n",
        )
        .expect("test precondition");

    let fingerprint = git_status_fingerprint(&root).expect("test precondition");

    let upstream = fingerprint.upstream.expect("test precondition");
    assert_eq!(upstream.remote, "included");
    assert_eq!(upstream.full_ref, "refs/remotes/included/main");
}

#[test]
fn git_status_fingerprint_reads_linked_worktree_config() {
    let _env = shepr_test_support::IsolatedEnv::new();
    let base = temp_test_dir("linked-worktree-config");
    let common_dir = base.join("repo/.git");
    let worktree = base.join("linked");
    let git_dir = common_dir.join("worktrees/linked");
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
            "[extensions]\n\tworktreeConfig = TRUE\n[remote \"fork\"]\n\tfetch = +refs/heads/*:refs/remotes/fork/*\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
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
    assert_eq!(upstream.full_ref, "refs/remotes/fork/main");
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
    assert_eq!(upstream.full_ref, "refs/remotes/upstream/main");
    assert_eq!(
        upstream.oid.as_deref(),
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
    assert_eq!(upstream.full_ref, "refs/remotes/origin/main");
    assert_eq!(
        upstream.oid.as_deref(),
        Some("2222222222222222222222222222222222222222")
    );
}
