//! Validated quality presets shared by the UI, capture and stream setup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LatencyMode {
    #[default]
    Interactive,
    Responsive,
    Buffered,
}

impl LatencyMode {
    pub const ALL: [Self; 3] = [Self::Interactive, Self::Responsive, Self::Buffered];

    pub fn label(self) -> &'static str {
        match self {
            Self::Interactive => "Interactive · 50 ms",
            Self::Responsive => "Responsive · 100 ms",
            Self::Buffered => "Buffered · 500 ms",
        }
    }

    pub fn millis(self) -> u64 {
        match self {
            Self::Interactive => 50,
            Self::Responsive => 100,
            Self::Buffered => 500,
        }
    }

    pub fn lead(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.millis())
    }

    pub fn audio_samples(self, rate: u32) -> u32 {
        (self.millis() * rate as u64 / 1000) as u32
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoQuality {
    Efficient,
    #[default]
    High,
    Smooth,
}

impl VideoQuality {
    pub const ALL: [Self; 3] = [Self::Efficient, Self::High, Self::Smooth];

    pub fn label(self) -> &'static str {
        match self {
            Self::Efficient => "720p / 30",
            Self::High => "1080p / 30",
            Self::Smooth => "1080p / 60",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Efficient => "Lower bandwidth and CPU use · up to 8 Mbps",
            Self::High => "Sharper text and detail · up to 20 Mbps",
            Self::Smooth => "Sharper text and smoother motion · up to 30 Mbps · higher CPU use",
        }
    }

    pub fn config(self) -> VideoConfig {
        match self {
            Self::Efficient => VideoConfig {
                width: 1280,
                height: 720,
                fps: 30,
                crf: 23,
                max_kbps: 8000,
                buffer_kbps: 2000,
                preset: "ultrafast",
            },
            Self::High => VideoConfig {
                width: 1920,
                height: 1080,
                fps: 30,
                crf: 18,
                max_kbps: 20000,
                buffer_kbps: 4000,
                preset: "veryfast",
            },
            Self::Smooth => VideoConfig {
                width: 1920,
                height: 1080,
                fps: 60,
                crf: 18,
                max_kbps: 30000,
                buffer_kbps: 6000,
                preset: "veryfast",
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub crf: u8,
    pub max_kbps: u32,
    pub buffer_kbps: u32,
    pub preset: &'static str,
}

impl VideoConfig {
    pub fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.width > 0
                && self.height > 0
                && self.width <= 3840
                && self.height <= 2160
                && self.width.is_multiple_of(2)
                && self.height.is_multiple_of(2),
            "Invalid video dimensions"
        );
        anyhow::ensure!(
            (1..=60).contains(&self.fps)
                && self.crf <= 51
                && self.max_kbps > 0
                && self.buffer_kbps > 0,
            "Invalid video encoding settings"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureMode {
    #[default]
    Mirror,
    Extend,
}

impl CaptureMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mirror => "Screen or window",
            Self::Extend => "Extended display",
        }
    }
}
