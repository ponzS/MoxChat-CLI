use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::{
    ecdsa::{
        signature::hazmat::{PrehashSigner, PrehashVerifier},
        Signature, SigningKey, VerifyingKey,
    },
    elliptic_curve::sec1::ToEncodedPoint,
    PublicKey, SecretKey,
};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroize;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn b64(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}
pub fn unb64(s: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s).context("Invalid base64url")
}
pub fn hash(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}
pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}
pub fn public(s: &SecretKey) -> String {
    let p = s.public_key().to_encoded_point(false);
    format!("{}.{}", b64(p.x().unwrap()), b64(p.y().unwrap()))
}
pub fn parse_public(s: &str) -> Result<PublicKey> {
    let (x, y) = s.split_once('.').context("Invalid Mox public key")?;
    let x = unb64(x)?;
    let y = unb64(y)?;
    ensure!(x.len() == 32 && y.len() == 32, "Invalid Mox public key");
    let mut bytes = vec![4];
    bytes.extend(x);
    bytes.extend(y);
    Ok(PublicKey::from_sec1_bytes(&bytes)?)
}

#[derive(Clone, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub public: String,
    pub private: String,
    pub epub: String,
    pub epriv: String,
    pub created_at: u64,
}
impl Identity {
    pub fn create(name: &str) -> Result<Self> {
        let name = name.trim();
        ensure!(
            !name.is_empty() && name.chars().count() <= 256,
            "昵称不能为空或超过 256 字符"
        );
        let signing = SecretKey::random(&mut OsRng);
        let encryption = SecretKey::random(&mut OsRng);
        Ok(Self {
            id: id(),
            name: name.into(),
            public: public(&signing),
            private: b64(signing.to_bytes()),
            epub: public(&encryption),
            epriv: b64(encryption.to_bytes()),
            created_at: now(),
        })
    }
    pub fn view(&self) -> Value {
        json!({"id":self.id,"name":self.name,"pub":self.public,"epub":self.epub})
    }
}
pub fn digest(domain: &str, value: &Value) -> Result<[u8; 32]> {
    let mut h = Sha256::new();
    h.update(domain.as_bytes());
    h.update([0]);
    h.update(serde_jcs::to_vec(value)?);
    Ok(h.finalize().into())
}
pub fn sign_digest(private: &str, digest: &[u8]) -> Result<String> {
    let raw = zeroize::Zeroizing::new(unb64(private)?);
    let key = SigningKey::from_slice(&raw)?;
    let sig: Signature = key.sign_prehash(digest)?;
    Ok(b64(sig.normalize_s().unwrap_or(sig).to_bytes()))
}
pub fn verify_digest(public: &str, signature: &str, digest: &[u8], low_s: bool) -> Result<()> {
    let key = VerifyingKey::from(parse_public(public)?);
    let sig = Signature::from_slice(&unb64(signature)?)?;
    ensure!(
        !low_s || sig.normalize_s().is_none(),
        "Noncanonical signature"
    );
    key.verify_prehash(digest, &sig)
        .map_err(|_| anyhow!("Signature verification failed"))
}
pub fn sign_document(domain: &str, mut value: Value, private: &str) -> Result<Value> {
    let signature = sign_digest(private, &digest(domain, &value)?)?;
    value
        .as_object_mut()
        .context("Expected object")?
        .insert("signature".into(), Value::String(signature));
    Ok(value)
}
pub fn verify_document(domain: &str, value: &Value, public: &str) -> Result<()> {
    let mut unsigned = value.clone();
    let sig = unsigned
        .as_object_mut()
        .context("Expected object")?
        .remove("signature")
        .context("Missing signature")?;
    verify_digest(
        public,
        sig.as_str().context("Invalid signature")?,
        &digest(domain, &unsigned)?,
        true,
    )
}
pub fn text_signature(text: &str, private: &str) -> Result<String> {
    sign_digest(
        private,
        &Sha256::digest(text.nfc().collect::<String>().trim().as_bytes()),
    )
}
pub fn verify_text(text: &str, signature: &str, public: &str) -> Result<()> {
    let digest = Sha256::digest(text.nfc().collect::<String>().trim().as_bytes());
    verify_digest(public, signature, &digest, false)
        .or_else(|_| verify_digest(public, signature, &Sha256::digest(digest), false))
}
fn shared_key(private: &str, public: &str) -> Result<[u8; 32]> {
    let private = SecretKey::from_slice(&unb64(private)?)?;
    let public = parse_public(public)?;
    let shared = p256::ecdh::diffie_hellman(private.to_nonzero_scalar(), public.as_affine());
    Ok(Sha256::digest(shared.raw_secret_bytes()).into())
}
pub fn encrypt(text: &str, private: &str, public: &str) -> Result<Value> {
    let key = zeroize::Zeroizing::new(shared_key(private, public)?);
    let iv = random::<12>();
    let cipher =
        Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| anyhow!("Invalid cipher key"))?;
    let ciphertext = cipher
        .encrypt(&Nonce::from(iv), text.as_bytes())
        .map_err(|_| anyhow!("Encryption failed"))?;
    Ok(json!({"ciphertext":b64(ciphertext),"iv":b64(iv)}))
}
pub fn decrypt(payload: &Value, private: &str, sender: &str) -> Result<String> {
    let key = zeroize::Zeroizing::new(shared_key(private, sender)?);
    let iv = unb64(string(payload, "iv")?)?;
    let iv: [u8; 12] = iv.try_into().map_err(|_| anyhow!("Invalid nonce"))?;
    let ct = unb64(string(payload, "ciphertext")?)?;
    let cipher =
        Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| anyhow!("Invalid cipher key"))?;
    let pt = cipher
        .decrypt(&Nonce::from(iv), ct.as_ref())
        .map_err(|_| anyhow!("Decryption failed"))?;
    Ok(String::from_utf8(pt)?)
}
pub fn string<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Missing or invalid {k}"))
}
pub struct SignedBody<'a> {
    pub text: &'a str,
    pub signature: &'a str,
}
pub fn envelope(
    identity: &Identity,
    to: &str,
    message_id: &str,
    kind: &str,
    body: SignedBody<'_>,
    recipient_epub: &str,
    created: u64,
) -> Result<Value> {
    let mut payload = encrypt(body.text, &identity.epriv, recipient_epub)?;
    let obj = payload.as_object_mut().unwrap();
    for (k,v) in json!({"sender":identity.epub,"timestamp":created,"messageId":message_id,"signature":body.signature,"sigAlg":"p256-sha256"}).as_object().unwrap(){obj.insert(k.clone(),v.clone());}
    let mut value = json!({"version":1,"kind":kind,"fromPub":identity.public,"toPub":to,"createdAt":created,"expiresAt":created+30*86400*1000,"messageId":message_id,"payload":payload});
    let d = digest("mox:mesh:envelope:v1", &value)?;
    value["envelopeId"] = json!(hex::encode(d));
    value["senderSignature"] = json!(sign_digest(&identity.private, &d)?);
    Ok(value)
}
pub fn verify_envelope(value: &Value) -> Result<()> {
    let mut projection = value.clone();
    let obj = projection.as_object_mut().context("Invalid envelope")?;
    obj.remove("envelopeId");
    obj.remove("senderSignature");
    obj.remove("transit");
    let d = digest("mox:mesh:envelope:v1", &projection)?;
    ensure!(
        value["envelopeId"].as_str() == Some(hex::encode(d).as_str()),
        "Envelope digest mismatch"
    );
    verify_digest(
        string(value, "fromPub")?,
        string(value, "senderSignature")?,
        &d,
        true,
    )
}
pub fn json_strict(text: &str) -> Result<Value> {
    // serde's Value parser overwrites duplicates. Detect them while visiting every object.
    struct Strict;
    impl<'de> serde::de::DeserializeSeed<'de> for Strict {
        type Value = Value;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<Value, D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de> serde::de::Visitor<'de> for Strict {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("JSON")
        }
        fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
            Ok(json!(v))
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
            if v.unsigned_abs() > 9007199254740991 {
                return Err(E::custom("Unsafe integer"));
            }
            Ok(json!(v))
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
            if v > 9007199254740991 {
                return Err(E::custom("Unsafe integer"));
            }
            Ok(json!(v))
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Value, E> {
            Err(E::custom("Noninteger JSON number"))
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
            Ok(json!(v))
        }
        fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<Value, E> {
            Ok(json!(v))
        }
        fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut a: A,
        ) -> std::result::Result<Value, A::Error> {
            let mut v = vec![];
            while let Some(x) = a.next_element_seed(Strict)? {
                v.push(x)
            }
            Ok(Value::Array(v))
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut a: A,
        ) -> std::result::Result<Value, A::Error> {
            let mut v = serde_json::Map::new();
            while let Some(k) = a.next_key::<String>()? {
                if v.contains_key(&k) {
                    return Err(serde::de::Error::custom("Duplicate JSON key"));
                }
                v.insert(k, a.next_value_seed(Strict)?);
            }
            Ok(Value::Object(v))
        }
    }
    use serde::de::DeserializeSeed;
    let mut d = serde_json::Deserializer::from_str(text);
    let v = Strict.deserialize(&mut d)?;
    d.end()?;
    Ok(v)
}
pub fn require_fields(v: &Value, required: &[&str], optional: &[&str]) -> Result<()> {
    let o = v.as_object().context("Expected object")?;
    if required.iter().any(|k| !o.contains_key(*k))
        || o.keys()
            .any(|k| !required.contains(&k.as_str()) && !optional.contains(&k.as_str()))
    {
        bail!("Invalid protocol fields")
    };
    Ok(())
}
