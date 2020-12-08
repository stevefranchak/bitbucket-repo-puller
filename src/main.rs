use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use clap::{crate_authors, crate_version, Clap};
use serde::Deserialize;
use tokio::fs::create_dir_all;
use tokio_compat_02::FutureExt;

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
        .compat()
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
        match get_clone_link_for_repo(&repo) {
            Some(link) => println!("Link for {}: {}", repo.name, link),
            None => println!("Repo {} has no clone link", repo.name),
        };
    }

    env::set_current_dir(orig_curr_dir)?;

    Ok(())
}
