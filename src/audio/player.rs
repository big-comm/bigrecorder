use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;

#[derive(Default)]
pub struct Player {
    pipeline: Option<gst::Pipeline>,
    current_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct PlaybackStatus {
    pub path: PathBuf,
    pub position: Duration,
    pub duration: Option<Duration>,
}

impl Player {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn toggle(&mut self, path: &Path) -> Result<()> {
        if self.is_playing_path(path) {
            self.stop()?;
        } else {
            self.play(path)?;
        }

        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        if let Some(pipeline) = self.pipeline.take() {
            pipeline
                .set_state(gst::State::Null)
                .map_err(|err| anyhow!("Failed to stop playback pipeline: {err:?}"))?;
        }
        self.current_path = None;

        Ok(())
    }

    pub fn poll_finished(&mut self) -> Result<bool> {
        let Some(pipeline) = self.pipeline.as_ref() else {
            return Ok(false);
        };
        let Some(bus) = pipeline.bus() else {
            return Ok(false);
        };

        let mut finished = false;
        let mut playback_error = None;
        while let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::ZERO,
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            match message.view() {
                gst::MessageView::Eos(_) => finished = true,
                gst::MessageView::Error(error) => {
                    playback_error = Some(anyhow!(
                        "Playback pipeline error: {} ({})",
                        error.error(),
                        error
                            .debug()
                            .map(|debug| debug.to_string())
                            .unwrap_or_else(|| "no debug details".to_string())
                    ));
                    finished = true;
                }
                _ => {}
            }
        }

        if finished {
            self.stop()?;
        }

        if let Some(error) = playback_error {
            return Err(error);
        }

        Ok(finished)
    }

    pub fn status(&self) -> Option<PlaybackStatus> {
        let pipeline = self.pipeline.as_ref()?;
        let path = self.current_path.clone()?;
        let position = pipeline
            .query_position::<gst::ClockTime>()
            .map(Duration::from)
            .unwrap_or(Duration::ZERO);
        let duration = pipeline
            .query_duration::<gst::ClockTime>()
            .map(Duration::from);

        Some(PlaybackStatus {
            path,
            position,
            duration,
        })
    }

    fn play(&mut self, path: &Path) -> Result<()> {
        self.stop()?;

        let uri = glib::filename_to_uri(path, None)
            .with_context(|| format!("Failed to build playback URI: {}", path.display()))?;
        let pipeline = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .build()
            .context("Missing GStreamer element: playbin")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("GStreamer playbin is not a pipeline"))?;
        // Recordings are audio only; skip the video and subtitle machinery.
        pipeline.set_property_from_str("flags", "audio+soft-volume");

        if let Err(err) = pipeline.set_state(gst::State::Playing) {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(anyhow!("Failed to start playback pipeline: {err:?}"));
        }

        self.current_path = Some(path.to_path_buf());
        self.pipeline = Some(pipeline);

        Ok(())
    }

    fn is_playing_path(&self, path: &Path) -> bool {
        self.current_path
            .as_deref()
            .is_some_and(|current_path| current_path == path)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(pipeline) = self.pipeline.take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }
}
