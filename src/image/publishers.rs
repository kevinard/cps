use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::{Image, Publisher, Publishers};
use crate::{model::validate_publisher, repo::catalog_entries};

pub struct PublisherInfo {
    pub name: String,
    pub repository: PathBuf,
}

pub fn list_publishers(root: &Path) -> Result<Vec<PublisherInfo>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    Ok(image
        .publishers()?
        .as_slice()
        .iter()
        .map(|publisher| PublisherInfo {
            name: publisher.name.clone(),
            repository: publisher.repo.clone(),
        })
        .collect())
}

pub fn add_publisher(root: &Path, name: &str, repo: &Path, key: &Path) -> Result<()> {
    let mut image = Image::open(root, true)?;
    image.require_clean()?;
    let configured = image.publishers()?;
    ensure!(
        configured.get(name).is_none(),
        "publisher {name} is already configured"
    );
    let publisher = Publisher::from_config(name, repo, key)?;
    catalog_entries(&publisher.repo, name, &publisher.key)
        .with_context(|| format!("verifying signed catalog for {name}"))?;
    let mut publishers = configured.as_slice().to_vec();
    publishers.push(publisher);
    save(&mut image, publishers)
}

pub fn prefer_publisher(root: &Path, name: &str) -> Result<()> {
    let mut image = Image::open(root, true)?;
    image.require_clean()?;
    let configured = image.publishers()?;
    let mut publishers = configured.as_slice().to_vec();
    let position = position(&publishers, name)?;
    if position == 0 {
        return Ok(());
    }
    publishers[..=position].rotate_right(1);
    save(&mut image, publishers)
}

pub fn remove_publisher(root: &Path, name: &str) -> Result<()> {
    let mut image = Image::open(root, true)?;
    image.require_clean()?;
    let configured = image.publishers()?;
    let mut publishers = configured.as_slice().to_vec();
    let position = position(&publishers, name)?;
    ensure!(
        publishers.len() > 1,
        "cannot remove the image's last publisher"
    );
    for (package_name, package) in image.installed()? {
        package.verify(&package_name, &configured)?;
        ensure!(
            package.published.manifest.publisher != name,
            "cannot remove publisher {name} while {package_name} is installed"
        );
    }
    publishers.remove(position);
    save(&mut image, publishers)
}

fn position(publishers: &[Publisher], name: &str) -> Result<usize> {
    validate_publisher(name)?;
    publishers
        .iter()
        .position(|publisher| publisher.name == name)
        .with_context(|| format!("publisher {name} is not configured"))
}

fn save(image: &mut Image, publishers: Vec<Publisher>) -> Result<()> {
    let publishers = Publishers::new(publishers)?;
    let mut settings = image.settings()?;
    settings.publisher_generation = settings
        .publisher_generation
        .checked_add(1)
        .context("publisher generation exhausted")?;
    let tx = image.db.transaction()?;
    tx.execute(
        "UPDATE publishers SET data=?1 WHERE id=1",
        [serde_json::to_string(&publishers)?],
    )?;
    tx.execute(
        "UPDATE image SET data=?1 WHERE id=1",
        [serde_json::to_string(&settings)?],
    )?;
    tx.commit()?;
    Ok(())
}
