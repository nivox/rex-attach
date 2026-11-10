//! Complete, transactional restoration using the published native decoder.
use super::*;

pub fn decode(bytes: &[u8], image_limit: u64) -> Result<Terminal> {
    let decoder = Owned::create(
        |out| {
            // SAFETY: bytes stay borrowed until the decoder is dropped below.
            unsafe {
                ffi::ghostty_snapshot_decoder_new_buf(
                    std::ptr::null(),
                    out,
                    bytes.as_ptr(),
                    bytes.len(),
                )
            }
        },
        ffi::ghostty_snapshot_decoder_free,
    )?;
    let continuation_limit = image_limit as usize;
    // SAFETY: both native options receive their documented uint64_t/size_t types.
    unsafe {
        check(ffi::ghostty_snapshot_decoder_set(
            decoder.raw(),
            ffi::SnapshotDecoderOption::KITTY_IMAGE_STORAGE_LIMIT,
            std::ptr::from_ref(&image_limit).cast(),
        ))?;
        check(ffi::ghostty_snapshot_decoder_set(
            decoder.raw(),
            ffi::SnapshotDecoderOption::MAX_CONTINUATION_BYTES,
            std::ptr::from_ref(&continuation_limit).cast(),
        ))?;
    }
    let terminal = Terminal(Owned::create(
        |out| {
            // SAFETY: complete decoding transfers ownership only after validation.
            unsafe { ffi::ghostty_snapshot_decoder_decode(decoder.raw(), out) }
        },
        ffi::ghostty_terminal_free,
    )?);
    // SAFETY: SOURCE_OFFSET writes a size_t after successful decoding.
    let consumed: usize = unsafe {
        query(|out| {
            ffi::ghostty_snapshot_decoder_get(
                decoder.raw(),
                ffi::SnapshotDecoderData::SOURCE_OFFSET,
                out,
            )
        })
    }?;
    ensure!(
        consumed == bytes.len(),
        "snapshot decoder did not consume the framed snapshot"
    );
    Ok(terminal)
}

#[cfg(test)]
pub fn encode(terminal: &Terminal) -> Result<Vec<u8>> {
    let mut length = 0;
    // SAFETY: the sizing pass accepts a null output buffer.
    let code = unsafe {
        ffi::ghostty_snapshot_encode_buf(terminal.raw(), std::ptr::null_mut(), 0, &mut length)
    };
    ensure!(
        code == ffi::Result::OUT_OF_SPACE || code == ffi::Result::SUCCESS,
        "snapshot sizing failed ({code})"
    );
    let mut bytes = vec![0; length];
    // SAFETY: the immutable terminal cannot change between sizing and encoding.
    check(unsafe {
        ffi::ghostty_snapshot_encode_buf(
            terminal.raw(),
            bytes.as_mut_ptr(),
            bytes.len(),
            &mut length,
        )
    })?;
    ensure!(length <= bytes.len(), "invalid encoded snapshot length");
    bytes.truncate(length);
    Ok(bytes)
}
