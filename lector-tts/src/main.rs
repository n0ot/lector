#[cfg(target_os = "macos")]
use anyhow::anyhow;
use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use cocoa::base::selector;
#[cfg(target_os = "macos")]
use cocoa_foundation::{
    base::id,
    foundation::{NSData, NSRunLoop},
};
#[cfg(target_os = "macos")]
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Object, Sel},
    sel, sel_impl,
};
#[cfg(target_os = "macos")]
use std::sync::Mutex;
#[cfg(not(target_os = "macos"))]
use std::io::BufRead;
use tts::Tts;

#[cfg(target_os = "macos")]
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    pub static NSFileHandleReadCompletionNotification: id;
    pub static NSFileHandleNotificationDataItem: id;
}

#[cfg(target_os = "macos")]
static IN_BUFFER: Mutex<String> = Mutex::new(String::new());

#[cfg(target_os = "macos")]
fn observe_stdin(tts: &mut Tts) -> Result<()> {
    unsafe {
        let nc: *mut Object = msg_send![class!(NSNotificationCenter), defaultCenter];
        let alloc_fh: *mut Object = msg_send![class!(NSFileHandle), alloc];
        let fh: *mut Object = msg_send![alloc_fh, initWithFileDescriptor: 0];
        let superclass = class!(NSObject);
        let mut file_observer_class = ClassDecl::new("FileObserver", superclass)
            .ok_or_else(|| anyhow!("declare Observer class"))?;
        extern "C" fn read_completed(this: &Object, _: Sel, notification: id) {
            unsafe {
                let user_info: *mut Object = msg_send![notification, userInfo];
                let data: id = msg_send![user_info, objectForKey: NSFileHandleNotificationDataItem];
                let ptr = data.bytes();
                let len = data.length();
                if ptr == std::ptr::null() {
                    // EOF
                    std::process::exit(0);
                }

                match std::str::from_utf8(std::slice::from_raw_parts(
                    ptr as *const u8,
                    len.try_into().unwrap(),
                )) {
                    Ok(s_slice) => {
                        let mut in_buffer_guard = IN_BUFFER.lock().unwrap();
                        in_buffer_guard.push_str(s_slice);

                        let tts_ptr_val: usize = *this.get_ivar("_tts_ptr");
                        let tts_callback = &mut *(tts_ptr_val as *mut Tts);

                        while let Some(pos) = in_buffer_guard.find('\n') {
                            let line = in_buffer_guard.drain(..=pos).collect::<String>();
                            // Drop the lock before calling handle_input if it could be re-entrant
                            // or very long-running. For this specific case, it's likely fine
                            // to hold it, but if handle_input could call back into something
                            // that tries to lock IN_BUFFER, this would need to be more careful.
                            if let Err(e) = handle_input(tts_callback, line.trim()) {
                                eprintln!("Error handling input: {}\nline: {}", e, line);
                                std::process::exit(1);
                            }
                        }
                        // Mutex guard is dropped here, releasing the lock.
                    }
                    Err(e) => {
                        eprintln!("Error decoding input: {}", e);
                        std::process::exit(1);
                    }
                }

                let fh: *mut Object = msg_send![notification, object];
                let _: () = msg_send![fh, readInBackgroundAndNotify];
            }
        }
        file_observer_class.add_method(
            selector("fileHandleReadCompleted:"),
            read_completed as extern "C" fn(&Object, Sel, id),
        );
        file_observer_class.add_ivar::<usize>("_tts_ptr");
        let file_observer_class = file_observer_class.register();
        let file_observer: *mut Object = msg_send![file_observer_class, new];
        file_observer
            .as_mut()
            .unwrap()
            .set_ivar("_tts_ptr", tts as *mut Tts as usize);
        let _: *mut Object = msg_send![nc, addObserver: file_observer  selector: selector("fileHandleReadCompleted:") name: NSFileHandleReadCompletionNotification object: fh];
        let _: () = msg_send![fh, readInBackgroundAndNotify];

        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn observe_stdin(tts: &mut Tts) -> Result<()> {
    for line in std::io::stdin().lock().lines() {
        handle_input(tts, line?.trim())?;
    }

    Ok(())
}

fn handle_input(tts: &mut Tts, input: &str) -> Result<()> {
    if input.is_empty() {
        return Ok(()); // Ignore empty input
    }

    match &input[0..1] {
        "r" => {
            let rate = input[1..].parse()?;
            tts.set_rate(rate)?;
        }
        "s" => _ = tts.speak(&input[1..], false)?,
        "x" => _ = tts.stop()?,
        _ => {}
    }

    Ok(())
}

fn main() -> Result<()> {
    let mut tts = Tts::default()?;
    observe_stdin(&mut tts).context("handle input")?;
    #[cfg(target_os = "macos")]
    unsafe {
        // Start the event loop
        let run_loop: id = NSRunLoop::currentRunLoop();
        let _: () = msg_send![run_loop, run];
    }

    Ok(())
}
