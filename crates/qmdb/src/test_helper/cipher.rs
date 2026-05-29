use aead::{Key, KeyInit};
use aes_gcm::Aes256Gcm;

pub fn create_cipher() -> Option<Aes256Gcm> {
    if cfg!(feature = "tee_cipher") {
        let key = Key::<Aes256Gcm>::from_slice(&[0; 32]);
        let cipher = Aes256Gcm::new(key);
        Some(cipher)
    } else {
        None
    }
}
