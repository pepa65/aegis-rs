extern crate serde_json;

use aes_gcm::{aead::AeadMut, Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose, Engine as _};
use color_eyre::eyre::Result;
use dialoguer::{theme::ColorfulTheme, FuzzySelect, Password};
use hex::FromHex;
use libreauth::oath::TOTPBuilder;
use password_hash::Output;
use scrypt::{
    password_hash::{PasswordHasher, SaltString},
    Scrypt,
};
use serde::Deserialize;
use serde_repr::Deserialize_repr;
use std::fs::File;
use std::io::Read;
use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Deserialize)]
struct EntryInfo {
    secret: String,
    algo: String,
    digits: i32,
    period: i32,
}

#[derive(Debug, Deserialize)]
struct Entry {
    r#type: String,
    uuid: String,
    name: String,
    issuer: String,
    note: String,
    favorite: bool,
    // #[serde(skip)]
    // icon: String,
    info: EntryInfo,
}

#[derive(Debug, Deserialize)]
struct Database {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct KeyParams {
    nonce: String,
    tag: String,
}

#[derive(Debug, Deserialize_repr, PartialEq)]
#[repr(u8)]
enum SlotType {
    Raw = 0,
    Password = 1,
    Biometric = 2,
}

#[derive(Debug, Deserialize)]
struct Slot {
    r#type: SlotType,
    uuid: String,
    key: String,
    key_params: KeyParams,
    n: Option<i32>,
    r: Option<i32>,
    p: Option<i32>,
    salt: Option<String>,
    repaired: Option<bool>,
    is_backup: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct Header {
    slots: Vec<Slot>,
    params: KeyParams,
}

#[derive(Debug, Deserialize)]
struct AegisBackup {
    version: u32,
    header: Header,
    db: String,
}

fn parse_aegis_json(path: &str) -> AegisBackup {
    let mut file = File::open(path).expect("Failed to open file");
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .expect("Failed to read file");
    serde_json::from_str(&contents).expect("Failed to parse JSON")
}

fn derive_key(password: &[u8], slot: &Slot) -> Output {
    let salt = SaltString::from_b64(
        &general_purpose::STANDARD_NO_PAD
            .encode(Vec::from_hex(&slot.salt.as_ref().unwrap()).expect("Failed to decode hex")),
    )
    .expect("Failed to parse salt");

    let n = (slot.n.unwrap() as f32).log2() as u8;
    let r = slot.r.unwrap() as u32;
    let p = slot.p.unwrap() as u32;

    let scrypt_params = scrypt::Params::new(n, r, p, 32).expect("Failed to set scrypt params");
    let derived_key = Scrypt
        .hash_password_customized(password, None, None, scrypt_params, &salt)
        .expect("Failed to derive key");

    derived_key.hash.expect("Failed to get hash of derived key")
}

fn generate_totp(secret: &str) -> Result<String> {
    let code = TOTPBuilder::new().base32_key(secret).finalize()?.generate();
    assert_eq!(code.len(), 6);
    Ok(code)
}

fn get_time_left(period: i32) -> i32 {
    let current_time = SystemTime::now();
    let seconds_since_epoch = current_time
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");
    let seconds = seconds_since_epoch.as_secs() as i32;
    let remaining_time = period - (seconds % period);

    remaining_time
}

fn decrypt_master_key(password: &str, slots: &Vec<Slot>) -> Option<Vec<u8>> {
    for slot in slots
        .iter()
        .filter(|s| s.r#type == SlotType::Password)
        .collect::<Vec<&Slot>>()
    {
        // Derive a key from the provided password and the salt from the file
        let derived_key = derive_key(password.as_bytes(), &slot);
        let key_nonce = Vec::from_hex(&slot.key_params.nonce).expect("Unexpected nonce format");
        let mut master_key_cipher = Vec::from_hex(&slot.key)
            .expect("Unexpected key format")
            .to_vec();
        master_key_cipher.extend_from_slice(
            &Vec::from_hex(&slot.key_params.tag).expect("Unexpected tag format"),
        );

        // Decrypt master key
        let mut cipher = Aes256Gcm::new(derived_key.as_bytes().into());
        let master_key = cipher
            .decrypt(Nonce::from_slice(&key_nonce), master_key_cipher.as_ref())
            .expect("Failed to decrypt master key");
        // TODO: Don't crash

        return Some(master_key);
    }

    None
}

fn decrypt_database(params: &KeyParams, master_key: &Vec<u8>, db: &String) -> Database {
    let db_nonce = Vec::from_hex(&params.nonce).expect("Unexpected nonce format");
    let db_tag = Vec::from_hex(&params.tag).expect("Unexpected tag format");
    let db_contents_cipher = general_purpose::STANDARD_NO_PAD
        .decode(db)
        .expect("Unexpected database format");

    let mut cipher_db = Aes256Gcm::new(master_key.as_slice().into());
    let mut db_cipher: Vec<u8> = db_contents_cipher;
    db_cipher.extend_from_slice(&db_tag);
    let db_contents = cipher_db
        .decrypt(Nonce::from_slice(&db_nonce), db_cipher.as_ref())
        .expect("Failed to decrypt database");

    let db_contents_str = String::from_utf8(db_contents).expect("Unexpected UTF-8 format");
    let db: Database = serde_json::from_str(&db_contents_str).expect("Failed to parse JSON");

    db
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let args: Vec<String> = env::args().collect();
    let aegis = parse_aegis_json(args.get(1).expect("No filepath argument"));

    if aegis.version != 1 {
        println!("Unsupported vault version: {}", aegis.version);
        std::process::exit(1);
    }

    let password = Password::with_theme(&ColorfulTheme::default())
        .with_prompt("Insert Aegis Password")
        .interact()?;
    // TODO: Support password file

    let master_key = decrypt_master_key(password.as_str(), &aegis.header.slots)
        .expect("Failed to decrypt master key");
    let db = decrypt_database(&aegis.header.params, &master_key, &aegis.db);

    if db.version != 2 {
        println!("Unsupported database version: {}", db.version);
        std::process::exit(1);
    }

    // TOTP Picker
    let items: Vec<String> = db
        .entries
        .iter()
        .map(|entry| format!("{} ({})", entry.issuer.trim(), entry.name.trim()))
        .collect();

    let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
        .items(&items)
        .default(0)
        .interact_opt()?;
    match selection {
        Some(index) => {
            let totp_info = &db.entries.get(index).unwrap().info;
            println!(
                "{}, ({}s left)",
                generate_totp(&totp_info.secret.as_str())?,
                get_time_left(totp_info.period)
            );
        }
        None => {
            println!("No selection");
        }
    }
    // TODO: Reset terminal on exit

    Ok(())
}
