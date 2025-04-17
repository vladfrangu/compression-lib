use napi::bindgen_prelude::{Buffer, Env, Result, Status};
use napi::Error;
use std::ptr::NonNull;
use zlib_rs::{
    c_api::z_stream,
    inflate::{self, InflateConfig, InflateStream},
    InflateFlush, ReturnCode,
};

const Z_SYNC_FLUSH_SUFFIX: &[u8] = &[0, 0, 255, 255];

#[napi]
struct ZlibDecompressor {
    chunk_size: u32,
    // Pointer to the heap-allocated z_stream
    stream_ptr: NonNull<z_stream>,
    // Buffer for incoming data until Z_SYNC_FLUSH
    internal_buffer: Vec<u8>,
    // Track finished state separately (for terminal errors or unexpected StreamEnd)
    finished: bool,
}

impl Drop for ZlibDecompressor {
    fn drop(&mut self) {
        // SAFETY: NonNull guarantees that the stream_ptr is valid. Additionally, since this is the Drop trait,
        // we should have no problems with double-frees or dangling pointers.
        unsafe {
            let _ = Box::from_raw(self.stream_ptr.as_ptr());
        }
    }
}

#[napi]
impl ZlibDecompressor {
    #[napi(constructor)]
    pub fn new(chunk_size: u32) -> Result<Self> {
        let mut stream = Box::new(z_stream::default());

        // Initialize the stream for inflation
        let config = InflateConfig::default(); // Use default window bits
        let ret_code = inflate::init(&mut *stream, config);
        if ret_code != ReturnCode::Ok {
            return Err(Error::new(
                Status::GenericFailure,
                format!("Failed to initialize inflate stream: {:?}", ret_code),
            ));
        }

        let stream_ptr = NonNull::new(Box::into_raw(stream)).ok_or_else(|| {
            // If this fails, something is very wrong (Box::into_raw returning null?)
            // We might need some manual deallocation logic here, but it's very complex so let's just pray for the best.
            Error::new(
                Status::GenericFailure,
                "Failed to get stream pointer after init",
            )
        })?;

        Ok(Self {
            stream_ptr,
            chunk_size,
            internal_buffer: Vec::new(),
            finished: false,
        })
    }

    #[napi(ts_return_type = "{ ok: true; data?: Buffer; } | { ok: false; error: string }")]
    pub fn push(&mut self, env: Env, data: Buffer) -> Result<napi::JsObject> {
        if self.finished {
            // Already finished (due to error or StreamEnd), return early
            let mut result_obj = env.create_object()?;
            result_obj.set_named_property("ok", env.get_boolean(true)?)?;
            return Ok(result_obj);
        }

        // Append new data to the internal buffer
        self.internal_buffer.extend_from_slice(&data);

        // Check if the buffer ends with the Z_SYNC_FLUSH suffix
        if !self.internal_buffer.ends_with(Z_SYNC_FLUSH_SUFFIX) {
            let mut result_obj = env.create_object()?;
            result_obj.set_named_property("ok", env.get_boolean(true)?)?;
            return Ok(result_obj);
        }

        // Flush suffix; take the buffer content for decompression
        let decompress = std::mem::take(&mut self.internal_buffer);

        // SAFETY: stream_ptr is valid and there is no way for there to be simultaneous writes to it.
        let stream = unsafe { self.stream_ptr.as_mut() };

        let mut input_chunk: &[u8] = &decompress;
        let mut output_buffer = Vec::new();
        let mut temp_out_buf = vec![0u8; self.chunk_size as usize];
        // Track if StreamEnd is hit unexpectedly
        let mut current_run_finished = false;

        while !input_chunk.is_empty() {
            stream.next_in = input_chunk.as_ptr() as *mut u8;
            stream.avail_in = input_chunk
                .len()
                .try_into()
                .map_err(|_| Error::new(Status::GenericFailure, "Input chunk too large"))?;

            loop {
                stream.next_out = temp_out_buf.as_mut_ptr();
                stream.avail_out = temp_out_buf.len().try_into().map_err(|_| {
                    Error::new(Status::GenericFailure, "Output chunk size too large")
                })?;

                let total_out_before_inflate = stream.total_out;
                let avail_in_before_inflate = stream.avail_in;

                // SAFETY: Our pointers are all valid
                let result_code = match unsafe { InflateStream::from_stream_mut(stream) } {
                    Some(inflate_stream_ref) => unsafe {
                        inflate::inflate(inflate_stream_ref, InflateFlush::NoFlush)
                    },
                    None => {
                        self.finished = true;
                        let mut error_obj = env.create_object()?;
                        error_obj.set_named_property("ok", env.get_boolean(false)?)?;
                        error_obj.set_named_property(
                            "error",
                            env.create_string("Failed to get inflate stream reference")?,
                        )?;
                        return Ok(error_obj);
                    }
                };

                let written_in_call = (stream.total_out - total_out_before_inflate) as usize;
                if written_in_call > 0 {
                    let actual_written = std::cmp::min(written_in_call, temp_out_buf.len());
                    output_buffer.extend_from_slice(&temp_out_buf[..actual_written]);
                }

                let consumed_in_call = (avail_in_before_inflate - stream.avail_in) as usize;
                input_chunk = &input_chunk[consumed_in_call..];

                match result_code {
                    ReturnCode::Ok => {
                        if stream.avail_out == 0 {
                            continue;
                        }

                        break;
                    }
                    // Discord shouldn't do this, but we handle it regardless
                    ReturnCode::StreamEnd => {
                        self.finished = true;
                        current_run_finished = true;
                        break;
                    }
                    // Should not happen with NoFlush, treat as unexpected or break
                    ReturnCode::BufError => {
                        // Assume it means output buffer is full
                        if stream.avail_out == 0 {
                            continue;
                        }
                        break;
                    }
                    other_code => {
                        self.finished = true;
                        let mut error_obj = env.create_object()?;
                        error_obj.set_named_property("ok", env.get_boolean(false)?)?;
                        error_obj.set_named_property(
                            "error",
                            env.create_string(&format!("Inflate error: {:?}", other_code))?,
                        )?;

                        return Ok(error_obj);
                    }
                }
            }

            if current_run_finished {
                break;
            }
        }

        let mut result_obj = env.create_object()?;
        result_obj.set_named_property("ok", env.get_boolean(true)?)?;
        if !output_buffer.is_empty() {
            result_obj.set_named_property(
                "data",
                env.create_buffer_with_data(output_buffer)?.into_raw(),
            )?;
        }

        Ok(result_obj)
    }
}
