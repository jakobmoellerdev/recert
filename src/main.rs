use crate::{cluster_crypto::scanning, ocp_postprocess::cluster_domain_rename::params::ClusterRenameParameters};
use anyhow::{Context, Result};
use clap::Parser;
use cluster_crypto::ClusterCryptoObjects;
use cnsanreplace::CnSanReplaceRules;
use etcd_client::Client as EtcdClient;
use k8s_etcd::InMemoryK8sEtcd;
use std::{path::PathBuf, sync::Arc};
use use_key::UseKeyRules;

mod cluster_crypto;
mod cnsanreplace;
mod file_utils;
mod json_tools;
mod k8s_etcd;

mod ocp_postprocess;
mod rsa_key_pool;
mod rules;
mod use_key;

/// A program to regenerate cluster certificates, keys and tokens
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    // etcd endpoint to recertify
    #[arg(long)]
    etcd_endpoint: String,

    /// Directory to recertify, such as /var/lib/kubelet, /etc/kubernetes and /etc/machine-config-daemon. Can specify multiple times
    #[arg(long)]
    static_dir: Vec<PathBuf>,

    /// A list of strings to replace in the subject name of all certificates. Can specify multiple.
    /// Must come in pairs of old and new values, separated by a space. For example:
    /// --cn-san-replace "foo bar" --cn-san-replace "baz qux" will replace all instances of "foo"
    /// with "bar" and all instances of "baz" with "qux" in the CN/SAN of all certificates.
    #[arg(long)]
    cn_san_replace: Vec<String>,

    /// Comma separated cluster name and cluster base domain.
    /// If given, many resources will be modified to use this new information
    #[arg(long)]
    cluster_rename: Option<String>,

    /// A list of CNs and the private keys to use for their certs.
    /// By default, new keys will be generated for all CNs, this option allows you to use existing
    /// keys instead.
    /// Must come in pairs of CN and private key file path, separated by a space. For example:
    /// --use-key "foo /etc/foo.key" --use-key "bar /etc/bar.key" will use the key in /etc/foo.key
    /// for certs with CN "foo" and the key in /etc/bar.key for certs with CN "bar".
    /// If more than one cert has the same CN, an error will occur and no certs will be regenerated.
    #[arg(long)]
    use_key: Vec<String>,

    /// Deprecated
    #[arg(long)]
    kubeconfig: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    main_internal(args).await
}

async fn main_internal(args: Cli) -> Result<()> {
    let (static_dirs, mut cluster_crypto, memory_etcd, cn_san_replace_rules, use_key_rules, cluster_rename) =
        init(args).await.context("initializing")?;

    // Scanning and recertification
    recertify(
        Arc::clone(&memory_etcd),
        &mut cluster_crypto,
        static_dirs.clone(),
        cn_san_replace_rules,
        use_key_rules,
    )
    .await
    .context("recertification")?;

    // Apply changes
    finalize(memory_etcd, &mut cluster_crypto, cluster_rename, static_dirs)
        .await
        .context("finalization")?;

    // Print summary
    cluster_crypto.display();

    Ok(())
}

async fn init(
    cli: Cli,
) -> Result<(
    Vec<PathBuf>,
    ClusterCryptoObjects,
    Arc<InMemoryK8sEtcd>,
    CnSanReplaceRules,
    UseKeyRules,
    Option<ClusterRenameParameters>,
)> {
    let etcd_client = EtcdClient::connect([cli.etcd_endpoint.as_str()], None)
        .await
        .context("connecting to etcd")?;

    // The main data structure for recording all crypto objects
    let cluster_crypto = ClusterCryptoObjects::new();

    // The in-memory etcd client, used for performance through caching etcd access
    let in_memory_etcd_client = Arc::new(InMemoryK8sEtcd::new(etcd_client));

    // User provided certificate CN/SAN domain name replacement rules
    let cn_san_replace_rules = CnSanReplaceRules::try_from(cli.cn_san_replace).context("parsing cli cn-san-replace")?;

    // User provided keys for particular CNs, when the user wants to use existing keys instead of
    // generating new ones
    let use_key_rules = UseKeyRules::try_from(cli.use_key).context("parsing cli use-key")?;

    Ok((
        cli.static_dir,
        cluster_crypto,
        in_memory_etcd_client,
        cn_san_replace_rules,
        use_key_rules,
        if let Some(cluster_rename) = cli.cluster_rename {
            Some(ClusterRenameParameters::try_from(cluster_rename)?)
        } else {
            None
        },
    ))
}

async fn recertify(
    in_memory_etcd_client: Arc<InMemoryK8sEtcd>,
    cluster_crypto: &mut ClusterCryptoObjects,
    static_dirs: Vec<PathBuf>,
    cn_san_replace_rules: CnSanReplaceRules,
    use_key_rules: UseKeyRules,
) -> Result<()> {
    // We want to scan the etcd and the filesystem in parallel to generating RSA keys as both take
    // a long time and are independent
    let all_discovered_crypto_objects = tokio::spawn(scanning::crypto_scan(in_memory_etcd_client, static_dirs));
    let rsa_keys = tokio::spawn(rsa_key_pool::RsaKeyPool::fill(300, 20));

    // Wait for the parallelizable tasks to finish and get their results
    let all_discovered_crypto_objects = all_discovered_crypto_objects.await?.context("scanning etcd/filesystem")?;
    let rsa_pool = rsa_keys.await?.context("generating rsa keys")?;

    cluster_crypto.register_discovered_crypto_objects(all_discovered_crypto_objects);

    establish_relationships(cluster_crypto)
        .await
        .context("establishing relationships")?;

    cluster_crypto
        .regenerate_crypto(rsa_pool, cn_san_replace_rules, use_key_rules)
        .context("regenerating cryptographic objects")?;

    Ok(())
}

async fn finalize(
    in_memory_etcd_client: Arc<InMemoryK8sEtcd>,
    cluster_crypto: &mut ClusterCryptoObjects,
    cluster_rename: Option<ClusterRenameParameters>,
    static_dirs: Vec<PathBuf>,
) -> Result<()> {
    cluster_crypto
        .commit_to_etcd_and_disk(&in_memory_etcd_client)
        .await
        .context("commiting the cryptographic objects back to memory etcd and to disk")?;
    ocp_postprocess(&in_memory_etcd_client, cluster_rename, static_dirs)
        .await
        .context("performing ocp specific post-processing")?;

    // Since we're using an in-memory fake etcd, we need to also commit the changes to the real
    // etcd after we're done
    in_memory_etcd_client
        .commit_to_actual_etcd()
        .await
        .context("commiting etcd cache to actual etcd")?;

    Ok(())
}

/// Perform some OCP-related post-processing to make some OCP operators happy
async fn ocp_postprocess(
    in_memory_etcd_client: &Arc<InMemoryK8sEtcd>,
    cluster_rename: Option<ClusterRenameParameters>,
    static_dirs: Vec<PathBuf>,
) -> Result<()> {
    println!("OCP postprocessing...");
    ocp_postprocess::fix_olm_secret_hash_annotation(in_memory_etcd_client)
        .await
        .context("fixing olm secret hash annotation")?;

    if let Some(cluster_rename) = cluster_rename {
        ocp_postprocess::cluster_rename(in_memory_etcd_client, cluster_rename, static_dirs)
            .await
            .context("renaming cluster")?;
    }

    Ok(())
}

async fn establish_relationships(cluster_crypto: &mut ClusterCryptoObjects) -> Result<()> {
    cluster_crypto.pair_certs_and_keys().context("pairing certs and keys")?;
    cluster_crypto.fill_cert_key_signers().context("filling cert signers")?;
    cluster_crypto.fill_jwt_signers().context("filling JWT signers")?;
    cluster_crypto.fill_signees().context("filling signees")?;
    cluster_crypto.associate_public_keys().context("associating public keys")?;
    Ok(())
}

#[cfg(test)]
mod tests;
