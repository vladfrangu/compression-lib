#![deny(clippy::all)]

use napi::bindgen_prelude::{Buffer, Env, Result, Status};
use napi::Error;
use std::marker::PhantomData;
use std::ptr::NonNull;
// Use the C API structures and functions from zlib-rs
use zlib_rs::{
    c_api::z_stream,                               // Removed Z_NULL and braces
    inflate::{self, InflateConfig, InflateStream}, // Need InflateStream for casting
    InflateFlush,
    ReturnCode,
};

#[macro_use]
extern crate napi_derive;

// TODO
const OUTPUT_CHUNK_SIZE: usize = 16 * 1024; // 16 KiB

#[napi]
struct Decompressor {
    // Pointer to the heap-allocated z_stream
    stream_ptr: NonNull<z_stream>,
    // Track finished state separately
    finished: bool,
}

impl Drop for Decompressor {
    fn drop(&mut self) {
        // SAFETY: NonNull guarantees that the stream_ptr is valid. Additionally, since this is the Drop trait,
        // we should have no problems with double-frees or dangling pointers.
        unsafe {
            let _ = Box::from_raw(self.stream_ptr.as_ptr());
        }
    }
}

#[napi]
impl Decompressor {
    #[napi(constructor)]
    pub fn new() -> Result<Self> {
        let mut stream_boxed = Box::new(z_stream::default());

        // Initialize the stream for inflation
        let config = InflateConfig::default(); // Use default window bits
        let ret_code = inflate::init(&mut *stream_boxed, config);
        if ret_code != ReturnCode::Ok {
            return Err(Error::new(
                Status::GenericFailure,
                format!("Failed to initialize inflate stream: {:?}", ret_code),
            ));
        }

        let stream_ptr = NonNull::new(Box::into_raw(stream_boxed)).ok_or_else(|| {
            // If this fails, something is very wrong (Box::into_raw returning null?)
            // We might need some manual deallocation logic here, but it's very complex so let's just pray for the best.
            Error::new(
                Status::GenericFailure,
                "Failed to get stream pointer after init",
            )
        })?;

        Ok(Self {
            stream_ptr,
            finished: false,
        })
    }

    #[napi(
        ts_return_type = "{ ok: true; data?: Buffer; finished: boolean } | { ok: false; error: string }"
    )]
    pub fn push(&mut self, env: Env, data: Buffer) -> Result<napi::JsObject> {
        if self.finished {
            // Return success but indicate finished and provide no data
            let mut result_obj = env.create_object()?;
            result_obj.set_named_property("ok", env.get_boolean(true)?)?;
            result_obj.set_named_property("finished", env.get_boolean(true)?)?;
            return Ok(result_obj);
        }

        let stream = unsafe { self.stream_ptr.as_mut() };

        let mut input_chunk: &[u8] = &data;
        let mut output_buffer = Vec::new();
        let mut temp_out_buf = [0u8; OUTPUT_CHUNK_SIZE]; // Use regular u8 slice

        // Keep track of total input consumed in this call
        let initial_total_in = stream.total_in;
        let initial_total_out = stream.total_out;

        let mut current_run_finished = false;

        loop {
            // If no more input for this push call, break the loop
            if input_chunk.is_empty() {
                break;
            }

            // Prepare the z_stream for the next inflate call
            stream.next_in = input_chunk.as_ptr() as *mut u8;
            stream.avail_in = input_chunk
                .len()
                .try_into()
                .map_err(|_| Error::new(Status::GenericFailure, "Input chunk too large"))?;
            stream.next_out = temp_out_buf.as_mut_ptr();
            stream.avail_out = temp_out_buf
                .len()
                .try_into()
                .map_err(|_| Error::new(Status::GenericFailure, "Output chunk size too large"))?;

            // Get a temporary InflateStream reference for the call
            // SAFETY: stream_ptr points to a valid, initialized z_stream.
            // State pointer inside should be valid if init succeeded.
            let result_code = match unsafe { InflateStream::from_stream_mut(stream) } {
                Some(inflate_stream_ref) => {
                    // SAFETY: We provide valid pointers and lengths.
                    unsafe { inflate::inflate(inflate_stream_ref, InflateFlush::NoFlush) }
                }
                None => {
                    // This should not happen if init succeeded and state is valid
                    self.finished = true; // Mark finished on error
                    let mut error_obj = env.create_object()?;
                    error_obj.set_named_property("ok", env.get_boolean(false)?)?;
                    error_obj.set_named_property(
                        "error",
                        env.create_string("Failed to get inflate stream reference")?,
                    )?;
                    return Ok(error_obj);
                }
            };

            let bytes_read = (stream.total_in - initial_total_in) as usize;
            let bytes_written_this_iteration = (stream.total_out - initial_total_out) as usize;

            // Calculate how many bytes were actually written into temp_out_buf in *this* inflate call
            let written_in_call = temp_out_buf.len() - (stream.avail_out as usize);
            if written_in_call > 0 {
                output_buffer.extend_from_slice(&temp_out_buf[..written_in_call]);
            }

            // Update input slice pointer based on avail_in change
            let consumed_in_call = input_chunk.len() - (stream.avail_in as usize);
            input_chunk = &input_chunk[consumed_in_call..];

            match result_code {
                ReturnCode::Ok => {
                    // Continue loop if input remains, or break if input for this push is consumed
                    if input_chunk.is_empty() {
                        break;
                    }
                    // If output buffer was full, loop again immediately
                    if stream.avail_out == 0 {
                        continue;
                    }
                    // Otherwise (input remains, output not full), something is unexpected?
                    // Maybe inflate stopped for internal reasons? Let's break and wait for next push.
                    break;
                }
                ReturnCode::StreamEnd => {
                    self.finished = true;
                    current_run_finished = true;
                    break; // Stream ended, stop processing.
                }
                ReturnCode::BufError => {
                    // Output buffer was full. We've copied the data.
                    // If input remains, loop again to process more.
                    if !input_chunk.is_empty() {
                        continue;
                    }
                    // If no input remains, break and wait for next push or finish.
                    break;
                }
                other_code => {
                    // An error occurred
                    self.finished = true; // Mark as finished on error
                    let mut error_obj = env.create_object()?;
                    error_obj.set_named_property("ok", env.get_boolean(false)?)?;
                    error_obj.set_named_property(
                        "error",
                        env.create_string(&format!("Inflate error: {:?}", other_code))?,
                    )?;
                    return Ok(error_obj);
                }
            }
        } // end loop

        // Create the success result object
        let mut result_obj = env.create_object()?;
        result_obj.set_named_property("ok", env.get_boolean(true)?)?;
        if !output_buffer.is_empty() {
            result_obj.set_named_property(
                "data",
                env.create_buffer_with_data(output_buffer)?.into_raw(),
            )?;
        }
        result_obj.set_named_property("finished", env.get_boolean(current_run_finished)?)?;

        Ok(result_obj)
    }

    #[napi(
        ts_return_type = "{ ok: true; data?: Buffer; finished: boolean } | { ok: false; error: string }"
    )]
    pub fn finish(&mut self, env: Env) -> Result<napi::JsObject> {
        if self.finished {
            // Already finished, return success with no data
            let mut result_obj = env.create_object()?;
            result_obj.set_named_property("ok", env.get_boolean(true)?)?;
            result_obj.set_named_property("finished", env.get_boolean(true)?)?;
            return Ok(result_obj);
        }

        let stream = unsafe { self.stream_ptr.as_mut() };

        let mut output_buffer = Vec::new();
        let mut temp_out_buf = [0u8; OUTPUT_CHUNK_SIZE];
        let initial_total_out = stream.total_out;
        let mut current_run_finished = false;

        loop {
            // Prepare the z_stream for the finish call (no input)
            stream.next_in = std::ptr::null_mut(); // No more input
            stream.avail_in = 0;
            stream.next_out = temp_out_buf.as_mut_ptr();
            stream.avail_out = temp_out_buf
                .len()
                .try_into()
                .map_err(|_| Error::new(Status::GenericFailure, "Output chunk size too large"))?;

            // Get a temporary InflateStream reference
            // SAFETY: stream_ptr points to a valid, initialized z_stream.
            let result_code = match unsafe { InflateStream::from_stream_mut(stream) } {
                Some(inflate_stream_ref) => {
                    // SAFETY: We provide valid pointers and lengths. Use Finish flush.
                    unsafe { inflate::inflate(inflate_stream_ref, InflateFlush::Finish) }
                }
                None => {
                    self.finished = true;
                    let mut error_obj = env.create_object()?;
                    error_obj.set_named_property("ok", env.get_boolean(false)?)?;
                    error_obj.set_named_property(
                        "error",
                        env.create_string("Failed to get inflate stream reference during finish")?,
                    )?;
                    return Ok(error_obj);
                }
            };

            // Calculate how many bytes were written into temp_out_buf in *this* inflate call
            let written_in_call = temp_out_buf.len() - (stream.avail_out as usize);
            if written_in_call > 0 {
                output_buffer.extend_from_slice(&temp_out_buf[..written_in_call]);
            }

            match result_code {
                ReturnCode::StreamEnd => {
                    self.finished = true; // Successfully finished
                    current_run_finished = true;
                    break;
                }
                ReturnCode::Ok => {
                    // Needs more calls to finish flushing? Continue loop.
                    // This happens if the output buffer wasn't large enough.
                    if written_in_call == 0 {
                        // If no bytes were written but still Ok, it might be finished
                        // without needing more output space this cycle, or something's stuck.
                        // Let's assume finished for safety if no progress.
                        self.finished = true;
                        current_run_finished = true; // Assume finished if OK and no output on flush
                        break;
                    }
                    // Otherwise, loop again.
                }
                ReturnCode::BufError => {
                    // Needs more output buffer space to finish. Loop again.
                    if written_in_call == 0 {
                        // If BufError and no bytes written, the buffer is genuinely too small.
                        self.finished = true; // Cannot proceed
                        let mut error_obj = env.create_object()?;
                        error_obj.set_named_property("ok", env.get_boolean(false)?)?;
                        error_obj.set_named_property(
                            "error",
                            env.create_string("Output buffer too small to finish inflation")?,
                        )?;
                        return Ok(error_obj);
                    }
                    // Otherwise, loop again to provide more output space
                }
                other_code => {
                    self.finished = true;
                    let mut error_obj = env.create_object()?;
                    error_obj.set_named_property("ok", env.get_boolean(false)?)?;
                    error_obj.set_named_property(
                        "error",
                        env.create_string(&format!("Inflate finish error: {:?}", other_code))?,
                    )?;
                    return Ok(error_obj);
                }
            }
        } // end loop

        let mut result_obj = env.create_object()?;
        result_obj.set_named_property("ok", env.get_boolean(true)?)?;
        if !output_buffer.is_empty() {
            result_obj.set_named_property(
                "data",
                env.create_buffer_with_data(output_buffer)?.into_raw(),
            )?;
        }
        result_obj.set_named_property("finished", env.get_boolean(current_run_finished)?)?;
        Ok(result_obj)
    }
}
