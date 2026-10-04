//! 默认 PoW 引擎：rquickjs（QuickJS）内执行挑战 JS。
//!
//! 执行序列与线上验证过的 Node/浏览器解法逐步对齐（fixtures/challenge_fresh.b64 重放）：
//! 1. 注入 `__goUserAgent/__goOrigin/__goStack/__goSha256Base64/__goBtoa/__goAtob` 六个宿主桩；
//! 2. 求值 DOM 桩 prelude.js（localStorage/crypto/TextEncoder/navigator…）；
//! 3. 求值服务端挑战（表达式，可能返回 Promise）并驱动 job 队列至落定；
//! 4. 把结果挂到 `__vqd_result`、计时挂到 `__vqdDurationMs`；
//! 5. 求值 mutation.js → JSON 文本 → base64 即 `X-Vqd-Hash-1` 请求头值。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rquickjs::function::Func;
use rquickjs::{Context, Runtime, Value};

use super::{
    MUTATION_JS, PRELUDE_JS, PowEngine, PowEnv, PowError, PowSolution, b64_decode, b64_encode,
    sha256_base64,
};

/// 单次求解的墙钟上限（与参考实现的 5s 超时对齐；触发 QuickJS interrupt）。
const SOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// 默认引擎：QuickJS 本地求解（§5.2）。
#[derive(Debug, Default, Clone, Copy)]
pub struct RquickjsPow;

impl PowEngine for RquickjsPow {
    fn solve(&self, challenge_b64: &str, env: &PowEnv) -> Result<PowSolution, PowError> {
        let decoded_bytes = b64_decode(challenge_b64)?;
        let decoded =
            String::from_utf8(decoded_bytes).map_err(|e| PowError::Decode(e.to_string()))?;

        let rt = Runtime::new().map_err(|e| PowError::Prelude(e.to_string()))?;
        let deadline = Instant::now() + SOLVE_TIMEOUT;
        let expired = Arc::new(AtomicBool::new(false));
        {
            let expired = expired.clone();
            rt.set_interrupt_handler(Some(Box::new(move || {
                let over = Instant::now() >= deadline;
                if over {
                    expired.store(true, Ordering::SeqCst);
                }
                over
            })));
        }

        let ctx = Context::full(&rt).map_err(|e| PowError::Prelude(e.to_string()))?;
        let solve_result = run_solve(&ctx, &decoded, env, &expired);

        if expired.load(Ordering::SeqCst) {
            return Err(PowError::Timeout);
        }
        solve_result
    }
}

fn run_solve(
    ctx: &Context,
    decoded: &str,
    env: &PowEnv,
    expired: &Arc<AtomicBool>,
) -> Result<PowSolution, PowError> {
    ctx.with(|ctx| {
        // ---- 1. 宿主桩 ------------------------------------------------
        let globals = ctx.globals();
        globals
            .set("__goUserAgent", env.user_agent.clone())
            .map_err(|e| PowError::Prelude(e.to_string()))?;
        globals
            .set("__goOrigin", env.origin.clone())
            .map_err(|e| PowError::Prelude(e.to_string()))?;
        globals
            .set("__goStack", env.stack.clone())
            .map_err(|e| PowError::Prelude(e.to_string()))?;
        globals
            .set(
                "__goSha256Base64",
                Func::from(|v: String| sha256_base64(&v)),
            )
            .map_err(|e| PowError::Prelude(e.to_string()))?;
        globals
            .set("__goBtoa", Func::from(btoa_js))
            .map_err(|e| PowError::Prelude(e.to_string()))?;
        globals
            .set("__goAtob", Func::from(atob_js))
            .map_err(|e| PowError::Prelude(e.to_string()))?;

        // ---- 2. DOM 桩 ------------------------------------------------
        ctx.eval::<Value<'_>, _>(PRELUDE_JS)
            .map_err(|e| wrap_js_error(expired, PowError::Prelude, e))?;

        // ---- 3. 挑战求解 ---------------------------------------------
        // 挑战是 async IIFE 表达式：普通 eval 求值（与 Node eval 同语义），再用
        // Promise.resolve 展开 thenable 并驱动 job 队列至落定。
        // 注意：不能直接用 eval_promise(JS_EVAL_FLAG_ASYNC)——该路径把程序完成值包成
        // `{value: X}` 且不展开内层 Promise（已在 debug_async_eval_shapes 钉住）。
        let started = Instant::now();
        let raw: Value<'_> = ctx
            .eval(decoded)
            .map_err(|e| wrap_js_error(expired, PowError::Eval, e))?;
        globals
            .set("__vqd_raw", raw)
            .map_err(|e| PowError::Eval(e.to_string()))?;
        let promise: rquickjs::Promise = ctx
            .eval("Promise.resolve(__vqd_raw)")
            .map_err(|e| wrap_js_error(expired, PowError::Eval, e))?;
        let value: Value<'_> = match promise.finish::<Value<'_>>() {
            Ok(v) => v,
            Err(rquickjs::Error::WouldBlock) => return Err(PowError::Unsettled("job queue drained".into())),
            Err(e) => return Err(wrap_js_error(expired, PowError::Eval, e)),
        };
        globals.set("__vqd_raw", ()).ok();

        // ---- 4. 注入结果与计时 --------------------------------------
        globals
            .set("__vqd_result", value)
            .map_err(|e| PowError::Eval(e.to_string()))?;
        globals
            .set("__vqdDurationMs", started.elapsed().as_millis().to_string())
            .map_err(|e| PowError::Eval(e.to_string()))?;

        // ---- 5. mutation → base64 ------------------------------------
        let payload: String = match ctx.eval::<String, _>(MUTATION_JS) {
            Ok(p) => p,
            Err(e) => {
                if expired.load(Ordering::SeqCst) {
                    return Err(PowError::Timeout);
                }
                // 失败时附带 __vqd_result 形态诊断，便于定位挑战输出结构变化
                let diag = ctx
                    .eval::<String, _>(
                        "(() => { try { const r = globalThis.__vqd_result; return typeof r + '|' + (r && typeof r === 'object' ? JSON.stringify(r).slice(0, 500) : String(r)); } catch (err) { return 'diag-fail ' + err; } })()",
                    )
                    .unwrap_or_else(|de| format!("diag-unavailable {de}"));
                return Err(PowError::Mutation(format!("{e} | diag={diag}")));
            }
        };
        let payload_b64 = b64_encode(payload.as_bytes());

        // 诊断信息（原始 client_hashes / challenge_id），任何失败都不影响主产物。
        let (raw_client_hashes, challenge_id) = ctx
            .eval::<String, _>(
                "JSON.stringify({h: __vqd_result.client_hashes, id: (__vqd_result.meta && __vqd_result.meta.challenge_id) || null})",
            )
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|v| {
                let hashes = v
                    .get("h")
                    .and_then(|h| h.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let id = v.get("id").and_then(|x| x.as_str()).map(str::to_string);
                (hashes, id)
            })
            .unwrap_or_default();

        Ok(PowSolution {
            payload_b64,
            raw_client_hashes,
            challenge_id,
            duration_ms: started.elapsed().as_millis().to_string(),
        })
    })
}

fn wrap_js_error(
    expired: &Arc<AtomicBool>,
    mk: fn(String) -> PowError,
    err: rquickjs::Error,
) -> PowError {
    if expired.load(Ordering::SeqCst) {
        PowError::Timeout
    } else {
        mk(err.to_string())
    }
}

/// JS `btoa`：按 latin1 取码位（>0xFF 的字符按浏览器语义视为非法，返回空串）。
fn btoa_js(input: String) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    for c in input.chars() {
        let cp = c as u32;
        if cp > 0xFF {
            return String::new();
        }
        bytes.push(cp as u8);
    }
    b64_encode(&bytes)
}

/// JS `atob`：base64 → latin1 字符串。
fn atob_js(input: String) -> String {
    match b64_decode(&input) {
        Ok(bytes) => bytes.into_iter().map(char::from).collect(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btoa_atob_roundtrip_ascii() {
        assert_eq!(btoa_js("hello".into()), "aGVsbG8=");
        assert_eq!(atob_js("aGVsbG8=".into()), "hello");
    }

    #[test]
    fn btoa_latin1_high_byte() {
        // JS: btoa("\u{e9}") == "6Q=="
        assert_eq!(btoa_js("\u{e9}".into()), "6Q==");
        assert_eq!(atob_js("6Q==".into()), "\u{e9}");
    }

    #[test]
    fn rejects_non_base64_challenge() {
        let env = PowEnv {
            user_agent: "ua".into(),
            origin: "https://duck.ai".into(),
            stack: "Error".into(),
        };
        assert!(matches!(
            RquickjsPow.solve("!!! not base64 !!!", &env),
            Err(PowError::Decode(_))
        ));
    }

    /// 回归钉：JS_EVAL_FLAG_ASYNC 的返回形态（`{value: X}` 包装且不展开内层 Promise）。
    /// 挑战求解绝不能走 eval_promise，必须普通 eval + Promise.resolve（见 run_solve）。
    #[test]
    fn debug_async_eval_shapes() {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            for src in [
                "({a:1})",
                "42",
                "return {b:2}",
                "(async () => ({c:3}))()",
                "Promise.resolve({d:4})",
            ] {
                let out = match ctx.eval_promise(src).and_then(|p| p.finish::<Value<'_>>()) {
                    Ok(v) => {
                        let _ = ctx.globals().set("__dbg_v", v);
                        ctx.eval::<String, _>("JSON.stringify(__dbg_v)")
                            .unwrap_or_else(|e| format!("stringify-err {e}"))
                    }
                    Err(e) => format!("ERR {e}"),
                };
                println!("src={src:?} => {out}");
            }
        });
    }
}
