//! Frozen zlib 1.3.1 encoder inverse. The caller accepts a profile only after a
//! complete byte comparison, and the output buffer is independently bounded.
use anyhow::{Result, ensure};
use std::{ffi::CStr, mem, ptr};
unsafe extern "C" fn allocate(
    _: libz_sys::voidpf,
    items: libz_sys::uInt,
    size: libz_sys::uInt,
) -> libz_sys::voidpf {
    // zlib checks a null allocation result. Paired with the same C allocator.
    unsafe { libc::calloc(items as usize, size as usize) }
}
unsafe extern "C" fn release(_: libz_sys::voidpf, address: libz_sys::voidpf) {
    unsafe { libc::free(address) }
}
struct Stream {
    stream: Box<libz_sys::z_stream>,
    initialized: bool,
}
impl Drop for Stream {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                libz_sys::deflateEnd(&mut *self.stream);
            }
        }
    }
}
pub fn encode(raw: &[u8], limit: usize, profile: [u8; 3]) -> Result<Vec<u8>> {
    let [level, memory, strategy] = profile;
    ensure!(
        level <= 9
            && (1..=9).contains(&memory)
            && strategy <= 4
            && raw.len() <= 32 * 1024 * 1024
            && limit <= 32 * 1024 * 1024,
        "native zlib budget/profile"
    );
    // The pointer is the linked library's immutable, NUL-terminated version.
    let version = unsafe { CStr::from_ptr(libz_sys::zlibVersion()) };
    ensure!(
        version.to_bytes() == b"1.3.1",
        "archive requires the pinned zlib 1.3.1 encoder"
    );
    let mut output = vec![0; limit];
    // Function pointers cannot be zero-initialized. The stream is boxed before
    // initialization because zlib keeps a pointer back to this exact address.
    let mut state = Stream {
        stream: Box::new(libz_sys::z_stream {
            next_in: raw.as_ptr().cast_mut(),
            avail_in: raw.len() as u32,
            total_in: 0,
            next_out: output.as_mut_ptr(),
            avail_out: output.len() as u32,
            total_out: 0,
            msg: ptr::null_mut(),
            state: ptr::null_mut(),
            zalloc: allocate,
            zfree: release,
            opaque: ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        }),
        initialized: false,
    };
    // Both buffers remain alive and stationary through deflateEnd. Deflate does
    // not modify next_in; the mutable pointer is a historical C API convention.
    let status = unsafe {
        libz_sys::deflateInit2_(
            &mut *state.stream,
            level as i32,
            8,
            15,
            memory as i32,
            strategy as i32,
            libz_sys::zlibVersion(),
            mem::size_of::<libz_sys::z_stream>() as i32,
        )
    };
    ensure!(
        status == libz_sys::Z_OK,
        "native zlib initialization failed"
    );
    state.initialized = true;
    let status = unsafe { libz_sys::deflate(&mut *state.stream, libz_sys::Z_FINISH) };
    ensure!(
        status == libz_sys::Z_STREAM_END && state.stream.avail_in == 0,
        "native zlib output limit or encoder failure"
    );
    let n = state.stream.total_out as usize;
    ensure!(n <= output.len(), "native zlib size");
    drop(state);
    output.truncate(n);
    Ok(output)
}
pub fn profiles(header: u8) -> Vec<[u8; 3]> {
    let levels: &[u8] = match header >> 6 {
        0 => &[1, 0],
        1 => &[3, 2],
        2 => &[6, 5, 4],
        _ => &[8, 9, 7],
    };
    let mut out = Vec::new();
    if header >> 6 == 3 {
        out.push([8, 9, 1]);
    }
    for strategy in [0, 1, 3, 2, 4] {
        if header >> 6 != 0 && strategy >= 2 {
            continue;
        }
        for &level in levels {
            for memory in [8, 9] {
                let p = [level, memory, strategy];
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn historical_encoder_profiles_are_portable() -> Result<()> {
        // Public compatibility vector, never a performance workload.
        let raw = (0..65536u32)
            .map(|i| (i ^ (i >> 7) ^ (i >> 13)) as u8)
            .collect::<Vec<_>>();
        let mut digest = blake3::Hasher::new();
        for profile in [[1, 8, 0], [6, 8, 0], [8, 9, 1], [9, 8, 1]] {
            let encoded = encode(&raw, raw.len() + 1024, profile)?;
            let mut decoded = Vec::new();
            flate2::read::ZlibDecoder::new(encoded.as_slice()).read_to_end(&mut decoded)?;
            assert_eq!(decoded, raw);
            digest.update(&encoded);
        }
        assert_eq!(
            digest.finalize().to_hex().as_str(),
            "a4886c16435f884f4f1e7003969db1fb4d77414852df83fa14e21d6e2fdaf1e1"
        );
        assert!(encode(&raw, 1, [6, 8, 0]).is_err());
        assert!(encode(&raw, 100, [10, 8, 0]).is_err());
        Ok(())
    }
}
