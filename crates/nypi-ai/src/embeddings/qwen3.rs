//! The Qwen3 decoder as a text encoder, over quantized GGUF weights: the hidden state of the last
//! token after the final norm, L2-normalized, is the embedding (Qwen3-Embedding's pooling).
//! candle's own quantized Qwen3 returns only next-token logits and keeps its layers private, so
//! the forward pass is written out here; it needs no KV cache, since each text is encoded whole.

use std::path::Path;

use anyhow::{Context, anyhow};
use candle_core::quantized::QTensor;
use candle_core::quantized::{QMatMul, gguf_file};
use candle_core::{DType, Device, Module, Tensor};

struct Layer {
    attn_norm: Tensor,
    q: QMatMul,
    k: QMatMul,
    v: QMatMul,
    o: QMatMul,
    q_norm: Tensor,
    k_norm: Tensor,
    ffn_norm: Tensor,
    gate: QMatMul,
    up: QMatMul,
    down: QMatMul,
}

pub struct Encoder {
    /// Dequantized to f16 to halve its memory; looked up by row, then widened.
    embed_tokens: Tensor,
    layers: Vec<Layer>,
    norm: Tensor,
    heads: usize,
    kv_heads: usize,
    head_dim: usize,
    eps: f32,
    rope_theta: f32,
    device: Device,
}

impl Encoder {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let device = pick_device();
        let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let content = gguf_file::Content::read(&mut file).map_err(|e| anyhow!("{e}"))?;
        let meta = |key: &str| content.metadata.get(key).ok_or_else(|| anyhow!("GGUF has no {key}"));
        let arch = meta("general.architecture")?.to_string().map_err(|e| anyhow!("{e}"))?.clone();
        let key = |name: &str| format!("{arch}.{name}");
        let heads = meta(&key("attention.head_count"))?.to_u32()? as usize;
        let kv_heads = meta(&key("attention.head_count_kv"))?.to_u32()? as usize;
        let hidden = meta(&key("embedding_length"))?.to_u32()? as usize;
        let head_dim = match meta(&key("attention.key_length")) {
            Ok(value) => value.to_u32()? as usize,
            Err(_) => hidden / heads,
        };
        let blocks = meta(&key("block_count"))?.to_u32()? as usize;
        let eps = meta(&key("attention.layer_norm_rms_epsilon"))?.to_f32()?;
        let rope_theta = meta(&key("rope.freq_base")).ok().and_then(|v| v.to_f32().ok()).unwrap_or(1_000_000.0);

        let mut tensor = |name: &str| -> anyhow::Result<QTensor> {
            content.tensor(&mut file, name, &device).with_context(|| name.to_string())
        };
        let embed_tokens = tensor("token_embd.weight")?.dequantize_f16(&device)?;
        let norm = tensor("output_norm.weight")?.dequantize(&device)?;
        let mut layers = Vec::with_capacity(blocks);
        for i in 0..blocks {
            let mut get = |name: &str| tensor(&format!("blk.{i}.{name}.weight"));
            let mut matmul = |name: &str| -> anyhow::Result<QMatMul> { Ok(QMatMul::from_qtensor(get(name)?)?) };
            let (q, k, v, o) = (matmul("attn_q")?, matmul("attn_k")?, matmul("attn_v")?, matmul("attn_output")?);
            let (gate, up, down) = (matmul("ffn_gate")?, matmul("ffn_up")?, matmul("ffn_down")?);
            let mut weight = |name: &str| -> anyhow::Result<Tensor> { Ok(get(name)?.dequantize(&device)?) };
            let (attn_norm, q_norm, k_norm, ffn_norm) =
                (weight("attn_norm")?, weight("attn_q_norm")?, weight("attn_k_norm")?, weight("ffn_norm")?);
            layers.push(Layer { attn_norm, q, k, v, o, q_norm, k_norm, ffn_norm, gate, up, down });
        }
        Ok(Encoder { embed_tokens, layers, norm, heads, kv_heads, head_dim, eps, rope_theta, device })
    }

    /// The normalized embedding of one tokenized text.
    pub fn embed(&self, ids: &[u32]) -> anyhow::Result<Vec<f32>> {
        let len = ids.len();
        if len == 0 {
            return Err(anyhow!("nothing to embed"));
        }
        let input = Tensor::new(ids, &self.device)?;
        let mut x = self.embed_tokens.index_select(&input, 0)?.to_dtype(DType::F32)?.unsqueeze(0)?;
        let (cos, sin) = self.rope(len)?;
        let mask = causal_mask(len, &self.device)?;
        for layer in &self.layers {
            let h = candle_nn::ops::rms_norm(&x, &layer.attn_norm, self.eps)?;
            let attended = self.attention(layer, &h, &cos, &sin, &mask)?;
            x = (x + attended)?;
            let h = candle_nn::ops::rms_norm(&x, &layer.ffn_norm, self.eps)?;
            let gated = (candle_nn::ops::silu(&layer.gate.forward(&h)?)? * layer.up.forward(&h)?)?;
            x = (x + layer.down.forward(&gated)?)?;
        }
        let x = candle_nn::ops::rms_norm(&x, &self.norm, self.eps)?;
        let last = x.squeeze(0)?.get(len - 1)?;
        let norm = last.sqr()?.sum_all()?.sqrt()?.to_scalar::<f32>()?.max(1e-12);
        Ok((last / norm as f64)?.to_vec1::<f32>()?)
    }

    fn attention(
        &self,
        layer: &Layer,
        h: &Tensor,
        cos: &Tensor,
        sin: &Tensor,
        mask: &Tensor,
    ) -> anyhow::Result<Tensor> {
        let (_, len, _) = h.dims3()?;
        let project = |m: &QMatMul, heads: usize, norm: Option<&Tensor>| -> anyhow::Result<Tensor> {
            let t = m.forward(h)?.reshape((1, len, heads, self.head_dim))?;
            // Qwen3 normalizes each head's queries and keys before the rotary embedding.
            let t = match norm {
                Some(weight) => candle_nn::ops::rms_norm(&t.contiguous()?, weight, self.eps)?,
                None => t,
            };
            Ok(t.transpose(1, 2)?.contiguous()?)
        };
        let q = candle_nn::rotary_emb::rope(&project(&layer.q, self.heads, Some(&layer.q_norm))?, cos, sin)?;
        let k = candle_nn::rotary_emb::rope(&project(&layer.k, self.kv_heads, Some(&layer.k_norm))?, cos, sin)?;
        let v = project(&layer.v, self.kv_heads, None)?;
        let (k, v) = (self.repeat_kv(k)?, self.repeat_kv(v)?);
        let scores = (q.matmul(&k.t()?)? / (self.head_dim as f64).sqrt())?.broadcast_add(mask)?;
        let weights = candle_nn::ops::softmax_last_dim(&scores)?;
        let out = weights.matmul(&v)?.transpose(1, 2)?.reshape((1, len, self.heads * self.head_dim))?;
        Ok(layer.o.forward(&out)?)
    }

    /// Grouped-query attention: each key/value head serves several query heads.
    fn repeat_kv(&self, t: Tensor) -> anyhow::Result<Tensor> {
        let repeat = self.heads / self.kv_heads;
        if repeat == 1 {
            return Ok(t);
        }
        let (b, kv, len, dim) = t.dims4()?;
        Ok(t.unsqueeze(2)?.expand((b, kv, repeat, len, dim))?.reshape((b, kv * repeat, len, dim))?)
    }

    fn rope(&self, len: usize) -> anyhow::Result<(Tensor, Tensor)> {
        let half = self.head_dim / 2;
        let inv_freq: Vec<f32> =
            (0..half).map(|i| 1.0 / self.rope_theta.powf(2.0 * i as f32 / self.head_dim as f32)).collect();
        let inv_freq = Tensor::new(inv_freq, &self.device)?.unsqueeze(0)?;
        let positions = Tensor::arange(0u32, len as u32, &self.device)?.to_dtype(DType::F32)?.unsqueeze(1)?;
        let freqs = positions.matmul(&inv_freq)?;
        Ok((freqs.cos()?.contiguous()?, freqs.sin()?.contiguous()?))
    }
}

fn causal_mask(len: usize, device: &Device) -> anyhow::Result<Tensor> {
    let mask: Vec<f32> =
        (0..len).flat_map(|i| (0..len).map(move |j| if j > i { f32::NEG_INFINITY } else { 0.0 })).collect();
    Ok(Tensor::from_vec(mask, (len, len), device)?)
}

/// Metal on Apple silicon when the system has the Metal API candle uses, else the CPU.
/// `NYPI_EMBED_DEVICE=cpu` forces the CPU.
fn pick_device() -> Device {
    if std::env::var("NYPI_EMBED_DEVICE").is_ok_and(|d| d.eq_ignore_ascii_case("cpu")) {
        return Device::Cpu;
    }
    // candle's Metal backend uses residency sets, which macOS 14 lacks; there it panics instead
    // of failing, so check the version rather than trying.
    #[cfg(target_os = "macos")]
    if macos_major() >= 15
        && let Ok(device) = Device::new_metal(0)
    {
        return device;
    }
    Device::Cpu
}

#[cfg(target_os = "macos")]
fn macos_major() -> u32 {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|v| v.trim().split('.').next()?.parse().ok())
        .unwrap_or(0)
}
