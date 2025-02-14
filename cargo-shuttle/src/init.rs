use std::fs::{self, read_to_string};
use std::num::NonZeroU32;
use std::sync::atomic::AtomicBool;
use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use gix::clone::PrepareFetch;
use gix::create::{self, Kind};
use gix::remote::fetch::Shallow;
use gix::{open, progress};
use regex::Regex;
use shuttle_common::constants::SHUTTLE_EXAMPLES_README;
use tempfile::{Builder, TempDir};
use toml_edit::{value, Document};
use url::Url;

use crate::args::TemplateLocation;

pub fn generate_project(dest: PathBuf, name: &str, temp_loc: TemplateLocation) -> Result<()> {
    println!(r#"Creating project "{name}" in "{}""#, dest.display());

    let temp_dir: TempDir = setup_template(&temp_loc.auto_path)
        .context("Failed to setup template generation directory")?;

    let path = match temp_loc.subfolder {
        Some(subfolder) => {
            let path = temp_dir.path().join(&subfolder);
            if path.exists() {
                path
            } else {
                anyhow::bail!(format!(
                    r#"There is no sub-folder "{}" in the template found at "{}""#,
                    subfolder, temp_loc.auto_path
                ))
            }
        }
        None => temp_dir.path().to_owned(),
    };

    // Prepare the template by changing its default contents.
    set_crate_name(&path, name).context("Failed to set crate name. No Cargo.toml in template?")?;
    edit_shuttle_toml(&path).context("Failed to edit Shuttle.toml")?;
    create_gitignore_file(&path).context("Failed to create .gitignore file")?;

    copy_dirs(&path, &dest, GitDir::Ignore)
        .context("Failed to copy the prepared template to the destination")?;

    drop(temp_dir);

    // Initialize a Git repository in the destination directory if there
    // is no existing Git repository present in the surrounding folders.
    let no_git_repo = gix::discover(&dest).is_err();
    if no_git_repo {
        gix::init(&dest).context("Failed to initialize project repository")?;
    }

    Ok(())
}

// Very loose restrictions are applied to repository names.
// What's important is that all names that are valid by the vendor's
// rules are accepted here. There is no need to check that the user
// actually provided a name that the vendor would accept.
const GIT_PATTERN: &str = "^(?:(?<vendor>gh|gl|bb):)?(?<owner>[^/.:]+)/(?<name>[^/.:]+)$";

/// Create a temporary directory and copy the template found at
/// `auto_path` into this directory. On success, a handle to this
/// directory is returned. It can then be used to modify the
/// template and lastly copy it to the actual destination.
fn setup_template(auto_path: &str) -> Result<TempDir> {
    let temp_dir = Builder::new()
        .prefix("cargo-shuttle-init")
        .tempdir()
        .context("Failed to create a temporary directory to generate the project into")?;

    let git_re = Regex::new(GIT_PATTERN).unwrap();

    if let Some(caps) = git_re.captures(auto_path) {
        let vendor = match caps.name("vendor").map(|v| v.as_str()) {
            Some("gl") => "https://gitlab.com/",
            Some("bb") => "https://bitbucket.org/",
            // GitHub is the default vendor if no other vendor is specified.
            Some("gh") | None => "https://github.com/",
            Some(_) => unreachable!("should never match unknown vendor"),
        };

        // `owner` and `name` are required for the regex to
        // match. Thus, we don't need to check if they exist.
        let url = format!("{vendor}{}/{}.git", &caps["owner"], &caps["name"]);
        gix_clone(&url, temp_dir.path())
            .with_context(|| format!("Failed to clone template Git repository at {url}"))?;
    } else if Path::new(auto_path).is_absolute() || auto_path.starts_with('.') {
        if Path::new(auto_path).exists() {
            copy_dirs(Path::new(auto_path), temp_dir.path(), GitDir::Copy)?;
        } else {
            anyhow::bail!(format!(
                "Local template directory \"{auto_path}\" with doesn't exist"
            ))
        }
    } else if let Ok(url) = auto_path.parse::<Url>() {
        if url.scheme() == "http" || url.scheme() == "https" {
            gix_clone(auto_path, temp_dir.path())
                .with_context(|| format!("Failed to clone Git repository at {url}"))?;
        } else {
            println!(
                "URL scheme is not supported. Please use HTTP of HTTPS for URLs, \
                or use another method of specifying the template location."
            );
            println!(
                "HINT: You can find examples of how to select a template here: {SHUTTLE_EXAMPLES_README}"
            );
            anyhow::bail!("invalid URL scheme")
        }
    } else {
        anyhow::bail!("template location is invalid")
    }

    Ok(temp_dir)
}

/// Mimic the behavior of `git clone`, cloning the Git repository found at
/// `from_url` into a directory `to_path`, using the API exposed by `gix`.
fn gix_clone(from_url: &str, to_path: &Path) -> Result<()> {
    let mut fetch = PrepareFetch::new(
        from_url,
        to_path,
        Kind::WithWorktree,
        create::Options {
            // Could be set to `true`, since we're always cloning into newly
            // created temporary directories. However, for this reason we
            // may just omit the requirement, and thereby omit another check
            // that might fail.
            destination_must_be_empty: false,
            fs_capabilities: None,
        },
        open::Options::isolated(),
    )
    .with_context(|| format!("Failed to prepare fetch repository '{from_url}'"))?
    .with_shallow(Shallow::DepthAtRemote(NonZeroU32::new(1).unwrap())); // Like `--depth 1`.

    let (mut prepare, _outcome) = fetch
        .fetch_then_checkout(progress::Discard, &AtomicBool::new(false))
        .with_context(|| format!("Failed to fetch repository '{from_url}'"))?;

    let (_repo, _outcome) = prepare
        .main_worktree(progress::Discard, &AtomicBool::new(false))
        .with_context(|| {
            format!(
                "Failed to checkout worktree of '{from_url}' into {}",
                to_path.display()
            )
        })?;

    Ok(())
}

/// Recursively copy all files and directories from `src` to `dest`. If
/// `git_policy` is set to `Ignore`, the `.git` directory is not copied.
/// If `git_policy` is set to `Copy`, then the `.git` directory is copied.
/// The procedure is the same as the one used in `cargo-generate`
/// https://github.com/cargo-generate/cargo-generate/blob/073b938b5205678bb25bd05aa8036b96ed5f22a7/src/lib.rs#L450
fn copy_dirs(src: &Path, dest: &Path, git_policy: GitDir) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let entry_type = entry.file_type()?;
        let entry_name = entry.file_name().to_string_lossy().to_string();

        let entry_dest = dest.join(&entry_name);

        if entry_type.is_dir() {
            if git_policy == GitDir::Ignore && entry_name == ".git" {
                continue;
            }

            // Recursion!
            copy_dirs(&entry.path(), &entry_dest, git_policy)?;
        } else if entry_type.is_file() {
            if entry_dest.exists() {
                println!(
                    "Warning: file '{}' already exists. Cannot overwrite",
                    entry_dest.display()
                );
            } else {
                // Copy this file.
                fs::copy(&entry.path(), &entry_dest)?;
            }
        } else if entry_type.is_symlink() {
            println!("Warning: symlink '{entry_name}' is ignored");
        }
    }

    Ok(())
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum GitDir {
    Ignore,
    Copy,
}

fn set_crate_name(path: &Path, name: &str) -> Result<()> {
    let path = path.join("Cargo.toml");
    let toml_str = read_to_string(&path)?;
    let mut doc = toml_str.parse::<Document>()?;

    // change the name
    doc["package"]["name"] = value(name);

    // write the file back out
    std::fs::write(&path, doc.to_string())?;

    Ok(())
}

fn edit_shuttle_toml(path: &Path) -> Result<()> {
    let path = path.join("Shuttle.toml");
    if !path.exists() {
        // Do nothing if template has no Shuttle.toml
        return Ok(());
    }
    let toml_str = read_to_string(&path)?;
    let mut doc = toml_str.parse::<Document>()?;

    // The Shuttle.toml project name override will likely already be in use,
    // so that field is not wanted in a newly cloned template.

    // remove the name
    doc.remove("name");

    if doc.len() == 0 {
        // if "name" was the only property in the doc, delete the file
        let _ = std::fs::remove_file(&path);

        return Ok(());
    }

    // write the file back out
    std::fs::write(&path, doc.to_string())?;

    Ok(())
}

/// Adds any missing recommended gitignore rules
fn create_gitignore_file(path: &Path) -> Result<()> {
    let path = path.join(".gitignore");
    let mut contents = std::fs::read_to_string(&path).unwrap_or_default();

    for rule in ["/target", ".shuttle-storage", "Secrets*.toml"] {
        if !contents.lines().any(|l| l == rule) {
            writeln!(&mut contents, "{rule}")?;
        }
    }

    std::fs::write(&path, contents)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gix_clone_works() {
        let temp_dir = Builder::new()
            .prefix("shuttle-clone-test")
            .tempdir()
            .unwrap();
        gix_clone(
            "https://github.com/shuttle-hq/shuttle-examples.git",
            temp_dir.path(),
        )
        .unwrap();
        // Check that some file we know to exist in the Repository exists in the clone.
        assert!(temp_dir.path().join("README.md").exists());
        temp_dir.close().unwrap();
    }

    #[test]
    fn copy_dirs_works() {
        let temp_dir = Builder::new()
            .prefix("shuttle-copy-test")
            .tempdir()
            .unwrap();
        let from = temp_dir.path().join("from");
        let with_git = temp_dir.path().join("with-git");
        let without_git = temp_dir.path().join("without-git");

        // First, create a normal copy of the test resource.
        copy_dirs(
            Path::new("tests/resources/copyable-project/"),
            &from,
            GitDir::Ignore,
        )
        .unwrap();
        assert!(from.join("src/main.rs").exists());
        assert!(from.join("Cargo.toml").exists());

        // Create a pseudo Git folder in the example project.
        std::fs::create_dir(from.join(".git")).unwrap();

        copy_dirs(&from, &with_git, GitDir::Copy).unwrap();
        assert!(with_git.join(".git").exists());
        assert!(with_git.join("src/main.rs").exists());
        assert!(with_git.join("Cargo.toml").exists());

        // Copy the same directory again, this time ignoring the `.git` folder.
        copy_dirs(&from, &without_git, GitDir::Ignore).unwrap();
        assert!(!without_git.join(".git").exists());
        assert!(without_git.join("src/main.rs").exists());
        assert!(without_git.join("Cargo.toml").exists());

        temp_dir.close().unwrap();
    }
}
