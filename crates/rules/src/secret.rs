use vuoxi_core::RelPath;

const SSH_KEY_PREFIXES: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
const SECRET_EXTS: &[&str] = &[
    "pem", "key", "kdbx", "kdb", "age", "gpg", "p12", "pfx", "jks", "keystore", "wallet",
];
const SECRET_NAMES: &[&str] = &["wallet.dat", "keystore.json", ".netrc", ".pgpass"];
const ENV_TEMPLATES: &[&str] = &[".env.example", ".env.sample", ".env.template", ".env.dist"];

/// Secret-looking files are `secrets` wherever they sit.
pub fn is_secret_file(rel: &RelPath) -> bool {
    let name = rel.file_name().to_ascii_lowercase();
    if name == ".env" || (name.starts_with(".env.") && !ENV_TEMPLATES.contains(&name.as_str())) {
        return true;
    }
    if SECRET_NAMES.contains(&name.as_str()) || name.starts_with("utc--") {
        return true;
    }
    if SSH_KEY_PREFIXES.iter().any(|p| name.starts_with(p)) && !name.ends_with(".pub") {
        return true;
    }
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| SECRET_EXTS.contains(&ext))
}

/// `.env` files only; used by detection to tell them from real key material.
pub(crate) fn is_env_file(rel: &RelPath) -> bool {
    rel.file_name().to_ascii_lowercase().starts_with(".env") && is_secret_file(rel)
}
