use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use clap::{crate_authors, crate_version, Clap};
use serde::Deserialize;
use tokio::fs::{create_dir_all, symlink_metadata};
use tokio::process::Command;

/// git clones all repos in a provided BitBucket project to a provided filesystem destination.
///
/// Required env var:
///
///   * BITBUCKET_ACCESS_TOKEN
#[derive(Clap)]
#[clap(version = crate_version!(), author = crate_authors!())]
struct Opts {
    #[clap(name = "BITBUCKET DOMAIN")]
    bitbucket_domain: String,
    #[clap(name = "BITBUCKET PROJECT")]
    bitbucket_project: String,
    #[clap(name = "TARGET DIRECTORY")]
    target_directory: String,
}

#[derive(Debug, Deserialize)]
struct BitBucketRepoListResult {
    size: usize,
    limit: usize,
    #[serde(rename(deserialize = "values"))]
    repos: Vec<BitBucketRepo>,
}

#[derive(Debug, Deserialize)]
struct BitBucketRepo {
    slug: String,
    name: String,
    links: HashMap<String, Vec<BitBucketRepoLink>>,
}

#[derive(Debug, Deserialize)]
struct BitBucketRepoLink {
    href: String,
    name: Option<String>,
}

static BITBUCKET_ACCESS_TOKEN_ENV_VAR_NAME: &str = "BITBUCKET_ACCESS_TOKEN";

#[derive(Debug)]
enum RepoActionState {
    ShouldClone,
    AlreadyCloned,
    CannotClone(std::io::Error),
}

fn get_access_token() -> Option<String> {
    match env::var(BITBUCKET_ACCESS_TOKEN_ENV_VAR_NAME) {
        Ok(token) => Option::Some(token),
        Err(_) => Option::None,
    }
}

async fn get_project_repos<T: AsRef<str>>(
    access_token: T,
    bitbucket_domain: T,
    bitbucket_project: T,
) -> Result<BitBucketRepoListResult, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    // TODO: support pagination/streaming
    let resp = client
        .get(&format!(
            "https://{}/rest/api/1.0/projects/{}/repos?limit=1000",
            bitbucket_domain.as_ref(),
            bitbucket_project.as_ref()
        ))
        .bearer_auth(access_token.as_ref())
        .send()
        .await?
        .json::<BitBucketRepoListResult>()
        .await?;
    Ok(resp)
}

fn get_clone_link_for_repo(repo: &BitBucketRepo) -> Option<String> {
    repo.links
        .get("clone")
        .unwrap()
        .iter()
        // TODO: parameterize link protocol (or is this filtering even needed?)
        .filter(|link| link.name.as_ref().unwrap_or(&String::from("")).as_str() == "ssh")
        .map(|link| link.href.clone())
        .next()
}

async fn ensure_target_directory_exists<T: AsRef<str>>(target_directory: T) {
    create_dir_all(target_directory.as_ref())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Could not create target directory {}",
                target_directory.as_ref()
            )
        })
}

fn remember_and_set_current_dir<T: AsRef<str>>(path: T) -> PathBuf {
    let orig_curr_dir = env::current_dir().expect("Unable to get current working directory");
    env::set_current_dir(path.as_ref()).unwrap_or_else(|_| {
        panic!(
            "Could not change current working directory to {}",
            path.as_ref()
        )
    });
    orig_curr_dir
}

// TODO: this needs a lot of refactoring - does the job for now though
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = Opts::parse();
    let access_token = get_access_token().expect("Missing env var BITBUCKET_ACCESS_TOKEN");

    // No point in making the request to BitBucket if we can't access the target directory
    ensure_target_directory_exists(&opts.target_directory).await;
    let orig_curr_dir = remember_and_set_current_dir(&opts.target_directory);

    let resp = get_project_repos(
        &access_token,
        &opts.bitbucket_domain,
        &opts.bitbucket_project,
    )
    .await?;

    println!("There are {} {} repos", resp.size, &opts.bitbucket_project);
    println!("-----\n");

    for repo in resp.repos {
        use std::io::{Error, ErrorKind};
        use RepoActionState::*;

        let repo_state = match symlink_metadata(&repo.slug).await {
            Ok(metadata) => {
                if metadata.is_dir() {
                    AlreadyCloned
                } else {
                    CannotClone(
                        Error::new(
                            ErrorKind::AlreadyExists, "{} exists on filesystem but is not a directory"
                        )
                    )
                }
            },
            Err(e) => {
                match e.kind() {
                    ErrorKind::NotFound => ShouldClone,
                    _ => CannotClone(e),
                }
            }
        };

        match repo_state {
            RepoActionState::CannotClone(e) => {
                eprintln!("Cannot clone {} at {}: {}", &repo.name, &repo.slug, e);
                continue;
            },
            RepoActionState::ShouldClone => {
                let repo_link = match get_clone_link_for_repo(&repo) {
                    Some(link) => link,
                    None => {
                        println!("Repo {} has no clone link", &repo.name);
                        continue;
                    }
                };
                let mut child = match Command::new("git").arg("clone").arg(&repo_link).spawn() {
                    Ok(child) => child,
                    Err(e) => {
                        eprintln!("Failed to execute 'git clone {}': {}", &repo_link, e);
                        continue;
                    }
                };
                match child.wait().await {
                    Ok(status) => match status.code() {
                        Some(code) if code != 0 => eprintln!("'git clone {}' failed with exit code {}", &repo_link, code),
                        Some(_) => (),
                        None => eprintln!("'git clone {}' was terminated by a signal", &repo_link)
                    },
                    Err(e) => {
                        eprintln!("Failed to execute 'git clone {}': {}", &repo_link, e);
                    }
                }
            },
            RepoActionState::AlreadyCloned => println!("{} ({}) is already cloned", &repo.name, &repo.slug),
        }

        env::set_current_dir(&repo.slug)?;

        let for_each_ref_output = match Command::new("git")
            .arg("for-each-ref")
            .arg("--sort=-committerdate")
            .arg("refs/remotes/origin")
            .arg("--format=%(refname:short)|%(committerdate)")
            .output()
            .await {
                Ok(output) => output,
                Err(e) => {
                    eprintln!("Failed to execute 'git for-each-ref': {}", e);
                    continue;
                }
            };

        // TODO: better handle error cases
        // TODO: use a tuple struct for the split line
        let mut filtered_output = std::str::from_utf8(&for_each_ref_output.stdout)?
            .lines()
            .map(|line| {
                let mut split_line = line.split("|");
                (split_line.next().unwrap().strip_prefix("origin/").unwrap(), split_line.next().unwrap())
            })
            .filter(|tuple| tuple.0 != "HEAD");

        if let Some(tuple) = filtered_output.next() {
            println!("Checking out branch '{}' ({}) for repo {}", &tuple.0, &tuple.1, &repo.slug);
            // TODO: make a function or macro
            // TODO: prefer 'git switch' if the installed git supports it
            let mut child = match Command::new("git").arg("checkout").arg(&tuple.0).spawn() {
                Ok(child) => child,
                Err(e) => {
                    eprintln!("Failed to execute 'git checkout {}': {}", &tuple.0, e);
                    continue;
                }
            };
            match child.wait().await {
                Ok(status) => match status.code() {
                    Some(code) if code != 0 => eprintln!("'git checkout {}' failed with exit code {}", &tuple.0, code),
                    Some(_) => (),
                    None => eprintln!("'git checkout {}' was terminated by a signal", &tuple.0)
                },
                Err(e) => {
                    eprintln!("Failed to execute 'git checkout {}': {}", &tuple.0, e);
                }
            }
            let mut child = match Command::new("git").arg("pull").spawn() {
                Ok(child) => child,
                Err(e) => {
                    eprintln!("Failed to execute 'git pull': {}", e);
                    continue;
                }
            };
            match child.wait().await {
                Ok(status) => match status.code() {
                    Some(code) if code != 0 => eprintln!("'git pull' failed with exit code {}", code),
                    Some(_) => (),
                    None => eprintln!("'git pull' was terminated by a signal")
                },
                Err(e) => {
                    eprintln!("Failed to execute 'git pull': {}", e);
                }
            }
        }

        env::set_current_dir(&opts.target_directory)?;
    }

    env::set_current_dir(orig_curr_dir)?;

    Ok(())
}
