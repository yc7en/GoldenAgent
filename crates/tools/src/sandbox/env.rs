use std::collections::HashSet;

/// 敏感关键词黑名单（大小写不敏感匹配）。
/// 只要环境变量名中包含这些词缀，物理剔除，绝对不向子进程继承。
const SENSITIVE_PATTERNS: &[&str] = &[
    "API_KEY",
    "APIKEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "AUTH",
    "CREDENTIAL",
    "PRIVATE",
    "ANTHROPIC",
    "OPENAI",
    "AWS_",
    "GITHUB_",
    "GOLDENAGENT_",
    "SSH_",
    "ACCESS_KEY",
];

/// 允许进入子进程的白名单环境变量（操作系统基础运行环境）。
#[cfg(windows)]
const ALLOWED_VARS: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "TEMP",
    "TMP",
    "COMSPEC",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "WINDIR",
    "OS",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_IDENTIFIER",
    "PUBLIC",
    "ALLUSERSPROFILE",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMPUTERNAME",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

#[cfg(not(windows))]
const ALLOWED_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TERM",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "PWD",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

/// 判定一个环境变量名是否敏感。
pub fn is_sensitive(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SENSITIVE_PATTERNS.iter().any(|&pattern| upper.contains(pattern))
}

/// 判定一个环境变量名是否在安全白名单中。
pub fn is_allowed(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ALLOWED_VARS.iter().any(|&allowed| allowed.eq_ignore_ascii_case(&upper))
}

/// 获取经过脱敏与白名单清洗后的安全环境变量列表。
pub fn get_sanitized_env() -> Vec<(String, String)> {
    let allowed_set: HashSet<String> = ALLOWED_VARS
        .iter()
        .map(|s| s.to_ascii_uppercase())
        .collect();

    let mut envs = Vec::new();

    for (k, v) in std::env::vars() {
        let upper_k = k.to_ascii_uppercase();
        // 1. 命中敏感模式的一律拦截
        if is_sensitive(&k) {
            continue;
        }
        // 2. 必须落在安全基础白名单内
        if allowed_set.contains(&upper_k) {
            envs.push((k, v));
        }
    }

    envs
}

/// 为 tokio Command 应用脱敏环境。
pub fn apply_to_tokio_command(cmd: &mut tokio::process::Command) {
    // 清空所有继承自父进程的环境变量
    cmd.env_clear();
    // 注入清洗后的安全白名单环境
    for (k, v) in get_sanitized_env() {
        cmd.env(k, v);
    }
}

/// 为 portable-pty CommandBuilder 应用脱敏环境。
pub fn apply_to_pty_command(cmd: &mut portable_pty::CommandBuilder) {
    cmd.env_clear();
    for (k, v) in get_sanitized_env() {
        cmd.env(k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sensitive_detection() {
        assert!(is_sensitive("OPENAI_API_KEY"));
        assert!(is_sensitive("ANTHROPIC_AUTH_TOKEN"));
        assert!(is_sensitive("MY_AWS_SECRET_ACCESS_KEY"));
        assert!(is_sensitive("GITHUB_TOKEN"));
        assert!(is_sensitive("goldenagent_password"));
        assert!(!is_sensitive("PATH"));
        assert!(!is_sensitive("USERPROFILE"));
    }

    #[test]
    fn test_sanitized_env_does_not_contain_secrets() {
        // 临时注入测试假 key
        std::env::set_var("TEST_OPENAI_API_KEY", "sk-1234567890");
        std::env::set_var("TEST_SECRET_TOKEN", "super-secret");

        let envs = get_sanitized_env();
        for (k, _) in envs {
            assert!(
                !is_sensitive(&k),
                "脱敏环境变量中绝对不能包含敏感键: {k}"
            );
        }

        std::env::remove_var("TEST_OPENAI_API_KEY");
        std::env::remove_var("TEST_SECRET_TOKEN");
    }
}
