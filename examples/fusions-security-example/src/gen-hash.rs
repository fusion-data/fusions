//! Generate argon2id hashes for seed data.
//!
//! Run: cargo run -p fusions-security-example --example gen-hash

use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng;
use argon2::{Argon2, PasswordHasher};

fn main() {
  for _ in 0..3 {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default().hash_password(b"password123", &salt).unwrap().to_string();
    println!("#1#{}", hash);
  }
}
