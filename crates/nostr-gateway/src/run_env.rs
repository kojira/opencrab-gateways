//! D-1091: この instance が core に宣言する、自分の binding 用の環境変数。
//!
//! core は名前も値も解釈せず、その binding のターンで `execute_shell` が起動する子へ
//! `env_clear()` の後にそのまま渡す。gateway だけが「この binding が誰のアカウントか」を
//! 知っているので、gateway が宣言する。
//!
//! 宣言するのは 2 つ:
//! - [`CONFIG_ENV`]: この instance の relays だけの config（鍵は載せない・[`crate::post`]）。
//! - [`crate::secret::SECRET_ENV`]: この instance の鍵。**プロセスが自分の instance を 1 つだけ
//!   持つときだけ**。daemon は instance ごとに別プロセスを起動して env で鍵を渡すので、
//!   その 1 つは「この instance の鍵」と言い切れる。1 プロセスが複数 instance を持つ配置では
//!   どの鍵がどれの物か区別できないため、**誰にも宣言しない**（他人の鍵を継承させない）。
//!
//! 鍵は値として map に載るだけで、config ファイルにも placement にも書かない。ログにも出さない
//! （[`declared_env_names`] が唯一のログ payload 源）。

use std::collections::BTreeMap;
use std::path::Path;

/// この instance の relays だけの config を指す env 名。
pub const CONFIG_ENV: &str = "NOSTARO_CONFIG";

/// プロセスの鍵がこの instance の物かどうか（[`crate::run::spawn_instance`] の入力）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretOwner {
    /// このプロセスはこの instance だけを持つ。プロセスの鍵はこの instance の鍵。
    ThisInstance,
    /// このプロセスは複数 instance を持つ。どの鍵がどれの物か区別できない。
    Ambiguous,
}

impl SecretOwner {
    /// instance 数から決める。daemon は instance あたり 1 プロセスを起動する
    /// （`daemon.rs` の `ExternalProcessSpawner`）ので、1 つなら自分の物。
    pub fn for_instance_count(instances: usize) -> Self {
        if instances == 1 {
            SecretOwner::ThisInstance
        } else {
            SecretOwner::Ambiguous
        }
    }

    /// この instance に鍵を宣言してよいか。
    pub fn declares_secret(self) -> bool {
        matches!(self, SecretOwner::ThisInstance)
    }
}

/// この instance の宣言（名前→値）。
///
/// `secret` は [`SecretOwner::ThisInstance`] のときだけ載せる。owner が曖昧なら
/// [`CONFIG_ENV`] だけを宣言する（config は instance ごとのパスなので他人の鍵は載らない）。
pub fn for_instance(
    config_path: &Path,
    owner: SecretOwner,
    secret: Option<&str>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        CONFIG_ENV.to_string(),
        config_path.to_string_lossy().into_owned(),
    );
    if owner.declares_secret() {
        if let Some(secret) = secret.filter(|s| !s.trim().is_empty()) {
            env.insert(crate::secret::SECRET_ENV.to_string(), secret.to_string());
        }
    }
    env
}

/// ログに載せてよい形（名前だけ）。値は絶対に含めない。
pub fn declared_env_names(env: &BTreeMap<String, String>) -> Vec<String> {
    env.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "nsec1do-not-log-this";

    #[test]
    fn declared_names_never_include_a_value() {
        let env = for_instance(
            Path::new("/x/cfg.toml"),
            SecretOwner::ThisInstance,
            Some(SECRET),
        );
        let names = declared_env_names(&env);
        assert_eq!(names.len(), 2);
        assert!(
            !names.iter().any(|n| n.contains("do-not-log")),
            "names only: {names:?}"
        );
    }

    #[test]
    fn own_instance_declares_the_config_and_its_own_key() {
        let env = for_instance(
            Path::new("/runtime/nostaro-post-1.toml"),
            SecretOwner::ThisInstance,
            Some(SECRET),
        );
        assert_eq!(
            env.get(CONFIG_ENV).map(String::as_str),
            Some("/runtime/nostaro-post-1.toml")
        );
        assert_eq!(
            env.get(crate::secret::SECRET_ENV).map(String::as_str),
            Some(SECRET)
        );
    }

    #[test]
    fn an_ambiguous_process_declares_no_key() {
        let env = for_instance(
            Path::new("/x/cfg.toml"),
            SecretOwner::Ambiguous,
            Some(SECRET),
        );
        assert!(!env.contains_key(crate::secret::SECRET_ENV));
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn two_instances_in_one_process_never_share_a_key() {
        let one = for_instance(
            Path::new("/x/one.toml"),
            SecretOwner::Ambiguous,
            Some("nsec1alpha"),
        );
        let two = for_instance(
            Path::new("/x/two.toml"),
            SecretOwner::Ambiguous,
            Some("nsec1beta"),
        );
        assert!(!one.contains_key(crate::secret::SECRET_ENV));
        assert!(!one.values().any(|v| v == "nsec1alpha" || v == "nsec1beta"));
        assert!(!two.contains_key(crate::secret::SECRET_ENV));
        assert_ne!(one.get(CONFIG_ENV), two.get(CONFIG_ENV));
    }

    #[test]
    fn no_secret_means_config_only() {
        let env = for_instance(Path::new("/x/cfg.toml"), SecretOwner::ThisInstance, None);
        assert_eq!(env.len(), 1);
        assert!(env.contains_key(CONFIG_ENV));
    }

    #[test]
    fn blank_secret_is_not_declared() {
        let env = for_instance(
            Path::new("/x/cfg.toml"),
            SecretOwner::ThisInstance,
            Some("   "),
        );
        assert!(!env.contains_key(crate::secret::SECRET_ENV));
    }

    #[test]
    fn owner_follows_the_instance_count() {
        assert!(SecretOwner::for_instance_count(1).declares_secret());
        assert!(!SecretOwner::for_instance_count(0).declares_secret());
        assert!(!SecretOwner::for_instance_count(2).declares_secret());
    }

    #[test]
    fn two_own_instances_get_their_own_key_and_config() {
        let alpha = for_instance(
            Path::new("/x/alpha.toml"),
            SecretOwner::ThisInstance,
            Some("nsec1alpha"),
        );
        let beta = for_instance(
            Path::new("/x/beta.toml"),
            SecretOwner::ThisInstance,
            Some("nsec1beta"),
        );
        assert_ne!(alpha, beta);
        assert_eq!(
            alpha.get(crate::secret::SECRET_ENV).map(String::as_str),
            Some("nsec1alpha")
        );
        assert_eq!(
            beta.get(crate::secret::SECRET_ENV).map(String::as_str),
            Some("nsec1beta")
        );
        assert_ne!(alpha.get(CONFIG_ENV), beta.get(CONFIG_ENV));
    }
}
