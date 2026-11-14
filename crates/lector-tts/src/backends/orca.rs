use oxilangtag::LanguageTag;
use tracing::{info, instrument, warn};
use zbus::{Connection, block_on, fdo::DBusProxy, names::BusName, proxy};

use crate::{Backend, Error, Features, SynthesizedAudio, UtteranceId, Voice};

const BUS_NAME: &str = "org.gnome.Orca1.Service";

#[proxy(
    interface = "org.gnome.Orca1.Service",
    default_service = "org.gnome.Orca1.Service",
    default_path = "/org/gnome/Orca1/Service"
)]
trait Service {
    fn speak_message(&self, message: &str) -> zbus::Result<bool>;

    fn get_version(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.gnome.Orca1.SpeechManager",
    default_service = "org.gnome.Orca1.Service",
    default_path = "/org/gnome/Orca1/Service/SpeechManager"
)]
trait SpeechManager {
    fn interrupt_speech(&self, notify_user: bool) -> zbus::Result<bool>;

    fn get_voices_for_language(
        &self,
        language: &str,
        variant: &str,
    ) -> zbus::Result<Vec<(String, String, String)>>;

    // Orca never emits `PropertiesChanged`, so caching would go permanently stale.
    #[zbus(property(emits_changed_signal = "false"))]
    fn rate(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn set_rate(&self, value: u32) -> zbus::Result<()>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn pitch(&self) -> zbus::Result<f64>;

    #[zbus(property)]
    fn set_pitch(&self, value: f64) -> zbus::Result<()>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn volume(&self) -> zbus::Result<f64>;

    #[zbus(property)]
    fn set_volume(&self, value: f64) -> zbus::Result<()>;

    #[zbus(property(emits_changed_signal = "false"))]
    fn current_voice(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn set_current_voice(&self, value: &str) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.a11y.Status",
    default_service = "org.a11y.Bus",
    default_path = "/org/a11y/bus"
)]
trait A11yStatus {
    #[zbus(property(emits_changed_signal = "false"))]
    fn screen_reader_enabled(&self) -> zbus::Result<bool>;
}

/// Returns whether the desktop reports an active screen reader via the
/// accessibility bus (`org.a11y.Status.ScreenReaderEnabled`).
#[instrument(level = "debug", ret)]
pub(crate) fn a11y_screen_reader_enabled() -> bool {
    fn probe() -> zbus::Result<bool> {
        let connection = block_on(Connection::session())?;
        block_on(async {
            A11yStatusProxy::new(&connection)
                .await?
                .screen_reader_enabled()
                .await
        })
    }
    match probe() {
        Ok(enabled) => enabled,
        Err(error) => {
            warn!(%error, "querying the accessibility bus failed");
            false
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Orca {
    service: ServiceProxy<'static>,
    speech: SpeechManagerProxy<'static>,
}

impl Orca {
    pub(crate) const NAME: &str = "Orca";

    /// Available only while Orca is running: its service is not D-Bus activatable.
    #[instrument(level = "debug", ret)]
    pub(crate) fn is_available() -> bool {
        fn probe() -> zbus::Result<bool> {
            let connection = block_on(Connection::session())?;
            let name = BusName::try_from(BUS_NAME)?;
            Ok(block_on(async {
                DBusProxy::new(&connection)
                    .await?
                    .name_has_owner(name)
                    .await
            })?)
        }
        match probe() {
            Ok(running) => running,
            Err(error) => {
                warn!(%error, "D-Bus probe for Orca failed");
                false
            }
        }
    }

    #[instrument(level = "info", err)]
    pub(crate) fn new() -> Result<Self, Error> {
        Self::from_connection(&block_on(Connection::session())?)
    }

    fn from_connection(connection: &Connection) -> Result<Self, Error> {
        let service = block_on(ServiceProxy::new(connection))?;
        let speech = block_on(SpeechManagerProxy::new(connection))?;
        let version = block_on(service.get_version()).map_err(|error| {
            warn!(%error, "Orca did not respond to GetVersion");
            Error::BackendUnavailable("Orca is not running")
        })?;
        info!(version, "Connected to Orca");
        Ok(Self { service, speech })
    }
}

impl Backend for Orca {
    #[instrument(level = "trace", skip(self))]
    fn supported_features(&self) -> Features {
        Features {
            stop: true,
            rate: true,
            pitch: true,
            volume: true,
            voice: true,
            get_voice: true,
            ..Default::default()
        }
    }

    #[instrument(level = "debug", skip(self), err)]
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<Option<UtteranceId>, Error> {
        // `SpeakMessage` queues rather than interrupts, so interruption is a separate call.
        if interrupt {
            block_on(self.speech.interrupt_speech(false))?;
        }
        // The returned boolean is unconditionally true; muted or disabled speech
        // silently drops the message.
        block_on(self.service.speak_message(text))?;
        Ok(None)
    }

    #[instrument(level = "debug", skip(self, _text), err)]
    fn synthesize(&mut self, _text: &str) -> Result<SynthesizedAudio, Error> {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err)]
    fn stop(&mut self) -> Result<(), Error> {
        block_on(self.speech.interrupt_speech(false))?;
        Ok(())
    }

    #[instrument(level = "debug", skip(self), err)]
    fn pause(&mut self) -> Result<(), Error> {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err)]
    fn resume(&mut self) -> Result<(), Error> {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self), err, ret)]
    fn is_paused(&self) -> Result<bool, Error> {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self))]
    fn min_rate(&self) -> f32 {
        0.
    }

    #[instrument(level = "trace", skip(self))]
    fn max_rate(&self) -> f32 {
        100.
    }

    #[instrument(level = "trace", skip(self))]
    fn normal_rate(&self) -> f32 {
        50.
    }

    #[allow(clippy::cast_precision_loss)]
    #[instrument(level = "debug", skip(self), err, ret)]
    fn get_rate(&self) -> Result<f32, Error> {
        Ok(block_on(self.speech.rate())? as f32)
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[instrument(level = "debug", skip(self), err)]
    fn set_rate(&mut self, rate: f32) -> Result<(), Error> {
        block_on(self.speech.set_rate(rate as u32))?;
        Ok(())
    }

    #[instrument(level = "trace", skip(self))]
    fn min_pitch(&self) -> f32 {
        0.
    }

    #[instrument(level = "trace", skip(self))]
    fn max_pitch(&self) -> f32 {
        10.
    }

    #[instrument(level = "trace", skip(self))]
    fn normal_pitch(&self) -> f32 {
        5.
    }

    #[allow(clippy::cast_possible_truncation)]
    #[instrument(level = "debug", skip(self), err, ret)]
    fn get_pitch(&self) -> Result<f32, Error> {
        Ok(block_on(self.speech.pitch())? as f32)
    }

    #[instrument(level = "debug", skip(self), err)]
    fn set_pitch(&mut self, pitch: f32) -> Result<(), Error> {
        block_on(self.speech.set_pitch(f64::from(pitch)))?;
        Ok(())
    }

    #[instrument(level = "trace", skip(self))]
    fn min_volume(&self) -> f32 {
        0.
    }

    #[instrument(level = "trace", skip(self))]
    fn max_volume(&self) -> f32 {
        10.
    }

    #[instrument(level = "trace", skip(self))]
    fn normal_volume(&self) -> f32 {
        10.
    }

    #[allow(clippy::cast_possible_truncation)]
    #[instrument(level = "debug", skip(self), err, ret)]
    fn get_volume(&self) -> Result<f32, Error> {
        Ok(block_on(self.speech.volume())? as f32)
    }

    #[instrument(level = "debug", skip(self), err)]
    fn set_volume(&mut self, volume: f32) -> Result<(), Error> {
        block_on(self.speech.set_volume(f64::from(volume)))?;
        Ok(())
    }

    #[instrument(level = "trace", skip(self), err, ret)]
    fn is_speaking(&self) -> Result<bool, Error> {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err)]
    fn voices(&self) -> Result<Vec<Voice>, Error> {
        // An empty language resolves to the current locale; Orca offers no way to
        // enumerate every voice together with a language tag.
        let families = block_on(self.speech.get_voices_for_language("", ""))?;
        Ok(families
            .into_iter()
            .filter_map(|(name, language, _variant)| {
                LanguageTag::parse(language).ok().map(|language| Voice {
                    id: name.clone(),
                    name,
                    gender: None,
                    language,
                })
            })
            .collect())
    }

    #[instrument(level = "debug", skip(self), err, ret)]
    fn voice(&self) -> Result<Option<Voice>, Error> {
        let name = block_on(self.speech.current_voice())?;
        if name.is_empty() {
            return Ok(None);
        }
        match self.voices()?.into_iter().find(|voice| voice.name == name) {
            Some(voice) => Ok(Some(voice)),
            None => Err(Error::VoiceNotFound(name)),
        }
    }

    #[instrument(level = "debug", skip(self), err)]
    fn set_voice(&mut self, voice: &Voice) -> Result<(), Error> {
        // Orca validates the name against its available voices and rejects the
        // write with a method error rather than a typed one.
        block_on(self.speech.set_current_voice(&voice.name)).map_err(|error| match error {
            zbus::Error::MethodError(..) => Error::VoiceNotFound(voice.name.clone()),
            // zbus 6 decodes standard remote errors instead of leaving them
            // as MethodError. Transport errors remain distinct.
            zbus::Error::FDO(ref remote) if !matches!(**remote, zbus::fdo::Error::ZBus(_)) => {
                Error::VoiceNotFound(voice.name.clone())
            }
            error => error.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::{os::unix::net::UnixStream, sync::Arc, thread, time::Duration};
    use zbus::{Guid, connection::Builder};

    #[derive(Default)]
    struct State {
        calls: Vec<String>,
        rate: u32,
        pitch: f64,
        volume: f64,
        voice: String,
    }

    struct ServiceFixture(Arc<Mutex<State>>);

    #[zbus::interface(name = "org.gnome.Orca1.Service")]
    impl ServiceFixture {
        #[allow(clippy::unused_self)] // D-Bus interface method.
        fn get_version(&self) -> &'static str {
            "fixture"
        }

        fn speak_message(&self, message: &str) -> bool {
            self.0.lock().calls.push(format!("speak:{message}"));
            true
        }
    }

    struct SpeechFixture(Arc<Mutex<State>>);

    #[zbus::interface(name = "org.gnome.Orca1.SpeechManager")]
    impl SpeechFixture {
        fn interrupt_speech(&self, notify_user: bool) -> bool {
            self.0.lock().calls.push(format!("interrupt:{notify_user}"));
            true
        }

        #[allow(clippy::unused_self)] // D-Bus interface method.
        fn get_voices_for_language(
            &self,
            language: &str,
            variant: &str,
        ) -> Vec<(String, String, String)> {
            assert!(language.is_empty() && variant.is_empty());
            ["one", "two"]
                .into_iter()
                .map(|name| (name.into(), "en-US".into(), String::new()))
                .collect()
        }

        #[zbus(property)]
        fn rate(&self) -> u32 {
            self.0.lock().rate
        }
        #[zbus(property)]
        fn set_rate(&mut self, value: u32) {
            self.0.lock().rate = value;
        }
        #[zbus(property)]
        fn pitch(&self) -> f64 {
            self.0.lock().pitch
        }
        #[zbus(property)]
        fn set_pitch(&mut self, value: f64) {
            self.0.lock().pitch = value;
        }
        #[zbus(property)]
        fn volume(&self) -> f64 {
            self.0.lock().volume
        }
        #[zbus(property)]
        fn set_volume(&mut self, value: f64) {
            self.0.lock().volume = value;
        }
        #[zbus(property)]
        fn current_voice(&self) -> String {
            self.0.lock().voice.clone()
        }
        #[zbus(property)]
        fn set_current_voice(&mut self, value: &str) -> zbus::fdo::Result<()> {
            if !["one", "two"].contains(&value) {
                return Err(zbus::fdo::Error::InvalidArgs("unknown voice".into()));
            }
            self.0.lock().voice = value.into();
            Ok(())
        }
    }

    #[test]
    fn proxy_calls_preserve_order_uncached_properties_and_remote_errors() {
        let state = Arc::new(Mutex::new(State {
            rate: 50,
            pitch: 5.0,
            volume: 10.0,
            voice: "one".into(),
            ..State::default()
        }));
        let (server_end, client_end) = UnixStream::pair().unwrap();
        let fixture = Arc::clone(&state);
        // Build both peers concurrently so authentication can make progress.
        // This uses only a private socket pair, never the desktop's D-Bus.
        let server = thread::spawn(move || {
            block_on(
                Builder::unix_stream(server_end)
                    .server(Guid::generate())
                    .p2p()
                    .serve_at(
                        "/org/gnome/Orca1/Service",
                        ServiceFixture(Arc::clone(&fixture)),
                    )
                    .serve_at(
                        "/org/gnome/Orca1/Service/SpeechManager",
                        SpeechFixture(fixture),
                    )
                    .build(),
            )
            .unwrap()
        });
        let connection = block_on(
            Builder::unix_stream(client_end)
                .p2p()
                .method_timeout(Duration::from_secs(2))
                .build(),
        )
        .unwrap();
        let server = server.join().unwrap();
        let mut backend = Orca::from_connection(&connection).unwrap();
        assert!(backend.speak("sample", true).unwrap().is_none());
        backend.stop().unwrap();
        assert_eq!(
            state.lock().calls,
            ["interrupt:false", "speak:sample", "interrupt:false"]
        );

        assert!((backend.get_rate().unwrap() - 50.0).abs() < f32::EPSILON);
        state.lock().rate = 67;
        assert!((backend.get_rate().unwrap() - 67.0).abs() < f32::EPSILON);
        backend.set_rate(42.0).unwrap();
        backend.set_pitch(2.5).unwrap();
        backend.set_volume(7.5).unwrap();
        assert!((backend.get_rate().unwrap() - 42.0).abs() < f32::EPSILON);
        assert!((backend.get_pitch().unwrap() - 2.5).abs() < f32::EPSILON);
        assert!((backend.get_volume().unwrap() - 7.5).abs() < f32::EPSILON);

        let voices = backend.voices().unwrap();
        assert_eq!(voices.len(), 2);
        backend.set_voice(&voices[1]).unwrap();
        assert_eq!(backend.voice().unwrap().unwrap().name, "two");
        let mut missing = voices[0].clone();
        missing.name = "missing".into();
        assert!(
            matches!(backend.set_voice(&missing), Err(Error::VoiceNotFound(name)) if name == "missing")
        );
        state.lock().voice = "missing".into();
        assert!(matches!(backend.voice(), Err(Error::VoiceNotFound(name)) if name == "missing"));

        block_on(server.close()).unwrap();
        assert!(!matches!(
            backend.set_voice(&voices[0]).unwrap_err(),
            Error::VoiceNotFound(_)
        ));
    }
}
