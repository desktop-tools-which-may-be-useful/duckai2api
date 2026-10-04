
(function () {
  if (!__vqd_result || typeof __vqd_result !== "object") {
    throw new Error("VQD hash script did not return an object");
  }
  if (!Array.isArray(__vqd_result.client_hashes)) {
    throw new Error("VQD hash script did not return client_hashes");
  }

  // SHA-256 hash each client_hash value (line 53719-53728)
  // 挑战返回什么值就 hash 什么值, 不做替换
  var hashed = __vqd_result.client_hashes.map(function (value) {
    return __goSha256Base64(String(value));
  });

  // 合并 meta (line 53729-53733): 保留挑战返回的 meta 字段, 覆盖 origin/stack/duration
  var meta = {};
  if (__vqd_result.meta && typeof __vqd_result.meta === "object") {
    for (var k in __vqd_result.meta) {
      if (Object.prototype.hasOwnProperty.call(__vqd_result.meta, k)) {
        meta[k] = __vqd_result.meta[k];
      }
    }
  }
  meta.origin = __goOrigin;
  meta.stack = __goStack;
  meta.duration = __vqdDurationMs;

  // 构造最终对象: 保留挑战返回的除 client_hashes/meta 外的其他字段
  var result = {};
  for (var k in __vqd_result) {
    if (Object.prototype.hasOwnProperty.call(__vqd_result, k)) {
      if (k !== "client_hashes" && k !== "meta") {
        result[k] = __vqd_result[k];
      }
    }
  }
  result.client_hashes = hashed;
  result.meta = meta;

  return JSON.stringify(result);
})();
