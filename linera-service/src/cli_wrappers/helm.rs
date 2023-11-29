// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use pathdiff::diff_paths;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub struct HelmRelease;

impl HelmRelease {
    pub async fn install(
        name: String,
        configs_dir: &PathBuf,
        server_config_id: usize,
        github_root: &Path,
        num_shards: usize,
        cluster_id: u32,
    ) -> Result<()> {
        let execution_dir = format!("{}/kubernetes/linera-validator", github_root.display());

        let configs_dir = diff_paths(configs_dir, execution_dir.clone())
            .context("Getting relative path failed")?;
        let configs_dir = configs_dir.to_str().expect("Getting str failed");

        let status = Command::new("helm")
            .current_dir(&execution_dir)
            .arg("install")
            .arg(&name)
            .arg(".")
            .args(["--values", "values-local.yaml"])
            .arg("--wait")
            .args(["--set", "installCRDs=true"])
            .args([
                "--set",
                &format!("validator.serverConfig={configs_dir}/server_{server_config_id}.json"),
            ])
            .args([
                "--set",
                &format!("validator.genesisConfig={configs_dir}/genesis.json"),
            ])
            .args(["--set", &format!("numShards={num_shards}")])
            .args(["--kube-context", &format!("kind-{}", cluster_id)])
            .args(["--timeout", "10m"])
            .status()
            .expect("Helm install should not fail!");

        if !status.success() {
            return Err(anyhow::anyhow!(
                "Error Helm installing release {} on cluster {}",
                name,
                cluster_id
            ));
        }

        Ok(())
    }
}
