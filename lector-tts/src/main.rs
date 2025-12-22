#[cfg(target_os = "macos")]
use anyhow::anyhow;
use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use objc2::{
    rc::Retained,
    runtime::{AnyObject, ClassBuilder, Sel},
    msg_send_id, sel, ClassType,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{
    NSData, NSFileHandle, NSFileHandleNotificationDataItem, NSFileHandleReadCompletionNotification,
    NSNotification, NSNotificationCenter, NSObject, NSRunLoop,
};
#[cfg(target_os = "macos")]
use std::sync::Mutex;
#[cfg(not(target_os = "macos"))]
use std::io::BufRead;
use tts::Tts;


#[cfg(target_os = "macos")]
static IN_BUFFER: Mutex<String> = Mutex::new(String::new());

#[cfg(target_os = "macos")]
fn observe_stdin(tts: &mut Tts) -> Result<()> {
    unsafe {
        let nc = NSNotificationCenter::defaultCenter();
        let fh = NSFileHandle::fileHandleWithStandardInput();
        let superclass = NSObject::class();
        let mut file_observer_class = ClassBuilder::new("FileObserver", superclass)
            .ok_or_else(|| anyhow!("declare Observer class"))?;
        extern "C" fn read_completed(this: &AnyObject, _: Sel, notification: &NSNotification) {
            unsafe {
                let user_info = notification
                    .userInfo()
                    .expect("notification should have user info");
                let data: Retained<NSData> = msg_send_id![&user_info, objectForKey: NSFileHandleNotificationDataItem]
                    .expect("user info should contain notification data")
                    .cast();
                let len = data.length();
                if len == 0 {
                    // EOF
                    std::process::exit(0);
                }

                match std::str::from_utf8(data.bytes()) {
                    Ok(s_slice) => {
                        let mut in_buffer_guard = IN_BUFFER.lock().unwrap();
                        in_buffer_guard.push_str(s_slice);

                        let tts_ptr_val: usize = *this.get_ivar::<usize>("_tts_ptr");
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

                let fh = notification.object().unwrap();
                let fh: Retained<NSFileHandle> = fh.cast();
                fh.readInBackgroundAndNotify();
            }
        }
        file_observer_class.add_method(
            sel!(fileHandleReadCompleted:),
            read_completed as extern "C" fn(&AnyObject, Sel, &NSNotification),
        );
        file_observer_class.add_ivar::<usize>("_tts_ptr");
        let file_observer_class = file_observer_class.register();
        let mut file_observer: Retained<AnyObject> = msg_send_id![file_observer_class, new];
        file_observer.set_ivar("_tts_ptr", tts as *mut Tts as usize);
        nc.addObserver_selector_name_object(
            &file_observer,
            sel!(fileHandleReadCompleted:),
            Some(NSFileHandleReadCompletionNotification),
            Some(&fh),
        );
        fh.readInBackgroundAndNotify();

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
    {
        // Start the event loop
        unsafe {
            let run_loop = NSRunLoop::currentRunLoop();
            run_loop.run();
        }
    }

    Ok(())
}
