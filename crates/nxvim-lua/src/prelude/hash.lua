-- nxvim:prelude/hash — the nx.hash.* hashing surface: one-shot digests of an
-- in-memory string, plus an incremental hasher for data that arrives in pieces.
-- (nx.hash.file — the streaming digest of a file on disk — is an fs op and lives
-- in fs.lua, alongside the other I/O.)
--
-- Every algorithm (sha1 / sha256 / sha512 / md5) is a pure-Rust RustCrypto hasher
-- behind the nx._hash / nx._hash_new bridges, so the whole surface is available on
-- every build (native and browser/wasm). Digests are returned as lowercase hex, and
-- inputs are hashed as RAW BYTES — binary data (NULs, non-UTF-8) hashes correctly.
--
-- Three ways to hash, by where the data lives:
--   * a whole in-memory string        -> nx.hash.sha256(s)         (one-shot, here)
--   * data that arrives in chunks      -> nx.hash.new("sha256")     (incremental, here)
--   * a file on disk (any size)        -> nx.hash.file(path)        (streamed, fs.lua)

nx.hash = nx.hash or {}

-- nx.hash.sha1(data) -> the SHA-1 digest of `data`, a 40-character lowercase-hex
-- string. `data` is hashed as raw bytes (binary-safe). Good for content addressing
-- and cache keys; NOT collision-resistant, so never rely on it for security.
function nx.hash.sha1(data)
  return nx._hash("sha1", data)
end

-- nx.hash.sha256(data) -> the SHA-256 digest of `data`, a 64-character lowercase-hex
-- string. `data` is hashed as raw bytes (binary-safe). The usual default for a
-- checksum or content hash.
function nx.hash.sha256(data)
  return nx._hash("sha256", data)
end

-- nx.hash.sha512(data) -> the SHA-512 digest of `data`, a 128-character lowercase-hex
-- string. `data` is hashed as raw bytes (binary-safe).
function nx.hash.sha512(data)
  return nx._hash("sha512", data)
end

-- nx.hash.md5(data) -> the MD5 digest of `data`, a 32-character lowercase-hex string.
-- `data` is hashed as raw bytes (binary-safe). For checksums and cache keys only —
-- MD5 is cryptographically broken; never use it for security.
function nx.hash.md5(data)
  return nx._hash("md5", data)
end

-- nx.hash.new(algo) -> an incremental hasher for data that arrives in pieces — a
-- subprocess's stdout, a download — so you can hash a stream as it flows without ever
-- holding it all in memory. `algo` is one of "sha1" / "sha256" / "sha512" / "md5"; an
-- unknown name errors here, at construction. The returned object has two methods:
--
-- ```
-- h:update(chunk)  -- fold more raw bytes in; call as many times as you like
-- h:hexdigest()    -- lowercase-hex digest of everything fed so far. NON-consuming:
--                     you may read an intermediate digest and keep updating after.
-- ```
--
-- Drive it from a stream with nx.await_each — feed each chunk in as it arrives:
--
-- ```lua
-- nx.async(function()
--   local h = nx.hash.new("sha256")
--   for batch in nx.await_each(nx.run_stream({ cmd = "curl", args = { "-s", url } })) do
--     for _, line in ipairs(batch) do h:update(line) end
--   end
--   print(h:hexdigest())
-- end)()
-- ```
--
-- Note nx.run_stream is line-oriented (newlines are stripped from each batch). For a
-- byte-exact digest of arbitrary binary output, feed the raw chunks from
-- nx.process.open's on_stdout instead. To digest a file on disk, prefer nx.hash.file,
-- which streams the file server-side and never sends its bytes to Lua.
function nx.hash.new(algo)
  return nx._hash_new(algo)
end
