//! 固定容量、单生产者单消费者的实时音频 FIFO。
//!
//! [`AudioFifo::split`] 返回彼此独立的生产者和消费者。生产者只能在一个线程中
//! 使用，消费者也只能在另一个线程中使用；两者之间不需要锁或阻塞等待。音频
//! 数据以 interleaved `f32` 样本传递，但容量、读写结果和可用空间均以 frame
//! 计数表示。

use rtrb::{Consumer, Producer, RingBuffer};
use thiserror::Error;

/// 创建音频 FIFO 时的配置错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FifoConfigError {
    /// FIFO 不能没有容量。
    #[error("FIFO 容量必须大于 0 frame")]
    ZeroCapacity,
    /// 每帧至少需要一个声道。
    #[error("声道数必须大于 0")]
    ZeroChannels,
    /// `capacity_frames * channels` 超出 `usize`。
    #[error("FIFO 样本容量溢出")]
    CapacityOverflow,
}

/// 输入样本切片无法表示完整的 interleaved 帧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("样本数 {samples} 不能被声道数 {channels} 整除")]
pub struct UnalignedSamples {
    /// 输入切片中的样本数。
    pub samples: usize,
    /// FIFO 固定的声道数。
    pub channels: usize,
}

/// 一次写入的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    /// 所有请求的帧都已写入。
    Complete { frames: usize },
    /// 只写入了部分请求帧，剩余帧因 FIFO 已满而未写入。
    Partial { frames: usize },
    /// 没有可用空间，未写入任何帧。
    Full,
}

impl PushResult {
    /// 返回实际写入的帧数。
    pub const fn frames(self) -> usize {
        match self {
            Self::Complete { frames } | Self::Partial { frames } => frames,
            Self::Full => 0,
        }
    }
}

/// 一次读取的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopResult {
    /// 目标切片已填满。
    Complete { frames: usize },
    /// 只读取了部分请求帧，剩余帧暂时不可用。
    Partial { frames: usize },
    /// 没有可读取的数据，目标切片保持不变。
    Empty,
}

impl PopResult {
    /// 返回实际读取的帧数。
    pub const fn frames(self) -> usize {
        match self {
            Self::Complete { frames } | Self::Partial { frames } => frames,
            Self::Empty => 0,
        }
    }
}

/// 固定容量 SPSC 音频 FIFO 的构造入口。
pub struct AudioFifo;

impl AudioFifo {
    /// 创建一个 FIFO，并返回唯一的生产者和消费者。
    ///
    /// `capacity_frames` 和 `channels` 在创建后固定。底层分配只发生在这里；
    /// [`AudioFifoProducer::push_interleaved`] 和
    /// [`AudioFifoConsumer::pop_interleaved`] 不分配、不阻塞。
    pub fn split(
        capacity_frames: usize,
        channels: usize,
    ) -> Result<(AudioFifoProducer, AudioFifoConsumer), FifoConfigError> {
        if capacity_frames == 0 {
            return Err(FifoConfigError::ZeroCapacity);
        }
        if channels == 0 {
            return Err(FifoConfigError::ZeroChannels);
        }
        let sample_capacity = capacity_frames
            .checked_mul(channels)
            .ok_or(FifoConfigError::CapacityOverflow)?;
        let (producer, consumer) = RingBuffer::new(sample_capacity);
        Ok((
            AudioFifoProducer {
                producer,
                capacity_frames,
                channels,
            },
            AudioFifoConsumer {
                consumer,
                capacity_frames,
                channels,
            },
        ))
    }
}

/// SPSC FIFO 的生产端，只应由一个线程持有和调用。
pub struct AudioFifoProducer {
    producer: Producer<f32>,
    capacity_frames: usize,
    channels: usize,
}

impl AudioFifoProducer {
    /// FIFO 的固定声道数。
    pub const fn channels(&self) -> usize {
        self.channels
    }

    /// FIFO 的固定容量（frame）。
    pub const fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    /// 当前可写入的 frame 数。该值可能随消费者线程运行而增加。
    pub fn available_frames(&self) -> usize {
        self.producer.slots() / self.channels
    }

    /// 将 interleaved `f32` 样本尽可能写入 FIFO。
    ///
    /// 输入必须包含完整帧。返回值明确区分全部写入、部分写入和 FIFO 已满；
    /// 输入切片本身不会被修改。切片为空时返回 `Complete { frames: 0 }`。
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Result<PushResult, UnalignedSamples> {
        if !samples.len().is_multiple_of(self.channels) {
            return Err(UnalignedSamples {
                samples: samples.len(),
                channels: self.channels,
            });
        }
        let requested_frames = samples.len() / self.channels;
        if requested_frames == 0 {
            return Ok(PushResult::Complete { frames: 0 });
        }

        let (written, _) = self.producer.push_partial_slice(samples);
        let written_frames = written.len() / self.channels;
        Ok(match written_frames {
            0 => PushResult::Full,
            n if n == requested_frames => PushResult::Complete { frames: n },
            n => PushResult::Partial { frames: n },
        })
    }
}

/// SPSC FIFO 的消费端，只应由一个线程持有和调用。
pub struct AudioFifoConsumer {
    consumer: Consumer<f32>,
    capacity_frames: usize,
    channels: usize,
}

impl AudioFifoConsumer {
    /// FIFO 的固定声道数。
    pub const fn channels(&self) -> usize {
        self.channels
    }

    /// FIFO 的固定容量（frame）。
    pub const fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    /// 当前可读取的 frame 数。该值可能随生产者线程运行而增加。
    pub fn available_frames(&self) -> usize {
        self.consumer.slots() / self.channels
    }

    /// 尽可能读取 interleaved `f32` 样本到调用方提供的切片。
    ///
    /// 目标切片必须包含完整帧。FIFO 为空时返回 `Empty`，数据不足时返回
    /// `Partial`；目标切片中未被写入的部分保持原值。
    pub fn pop_interleaved(&mut self, samples: &mut [f32]) -> Result<PopResult, UnalignedSamples> {
        if !samples.len().is_multiple_of(self.channels) {
            return Err(UnalignedSamples {
                samples: samples.len(),
                channels: self.channels,
            });
        }
        let requested_frames = samples.len() / self.channels;
        if requested_frames == 0 {
            return Ok(PopResult::Complete { frames: 0 });
        }

        let (read, _) = self.consumer.pop_partial_slice(samples);
        let read_frames = read.len() / self.channels;
        Ok(match read_frames {
            0 => PopResult::Empty,
            n if n == requested_frames => PopResult::Complete { frames: n },
            n => PopResult::Partial { frames: n },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn rejects_invalid_configuration() {
        assert!(matches!(
            AudioFifo::split(0, 2),
            Err(FifoConfigError::ZeroCapacity)
        ));
        assert!(matches!(
            AudioFifo::split(4, 0),
            Err(FifoConfigError::ZeroChannels)
        ));
        assert!(matches!(
            AudioFifo::split(usize::MAX, 2),
            Err(FifoConfigError::CapacityOverflow)
        ));
    }

    #[test]
    fn preserves_order_and_reports_boundaries() {
        let (mut producer, mut consumer) = AudioFifo::split(2, 2).unwrap();
        assert_eq!(
            consumer.pop_interleaved(&mut [9.0, 9.0]).unwrap(),
            PopResult::Empty
        );
        assert_eq!(
            producer.push_interleaved(&[1.0, 2.0, 3.0, 4.0]).unwrap(),
            PushResult::Complete { frames: 2 }
        );
        assert_eq!(producer.available_frames(), 0);
        assert_eq!(
            producer.push_interleaved(&[5.0, 6.0]).unwrap(),
            PushResult::Full
        );
        let mut output = [0.0; 4];
        assert_eq!(
            consumer.pop_interleaved(&mut output).unwrap(),
            PopResult::Complete { frames: 2 }
        );
        assert_eq!(output, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(consumer.available_frames(), 0);
    }

    #[test]
    fn supports_partial_reads_and_writes() {
        let (mut producer, mut consumer) = AudioFifo::split(2, 2).unwrap();
        assert_eq!(
            producer
                .push_interleaved(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
                .unwrap(),
            PushResult::Partial { frames: 2 }
        );
        let mut output = [0.0; 6];
        assert_eq!(
            consumer.pop_interleaved(&mut output).unwrap(),
            PopResult::Partial { frames: 2 }
        );
        assert_eq!(&output[..4], &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(
            producer.push_interleaved(&[5.0, 6.0, 7.0, 8.0]).unwrap(),
            PushResult::Complete { frames: 2 }
        );
        assert_eq!(&output[..4], &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(
            consumer.pop_interleaved(&mut output[4..]).unwrap(),
            PopResult::Complete { frames: 1 }
        );
        assert_eq!(&output[4..], &[5.0, 6.0]);
    }

    #[test]
    fn rejects_unaligned_slices_without_mutation() {
        let (mut producer, mut consumer) = AudioFifo::split(4, 2).unwrap();
        assert_eq!(
            producer.push_interleaved(&[1.0]).unwrap_err(),
            UnalignedSamples {
                samples: 1,
                channels: 2
            }
        );
        let mut output = [7.0; 3];
        assert_eq!(
            consumer.pop_interleaved(&mut output).unwrap_err(),
            UnalignedSamples {
                samples: 3,
                channels: 2
            }
        );
        assert_eq!(output, [7.0; 3]);
    }

    #[test]
    fn preserves_order_across_threads() {
        let (mut producer, mut consumer) = AudioFifo::split(8, 2).unwrap();
        let expected: Vec<f32> = (0..2_000).map(|n| n as f32).collect();
        let producer_expected = expected.clone();
        let expected_len = expected.len();
        let writer = thread::spawn(move || {
            for chunk in producer_expected.chunks(14) {
                let mut offset = 0;
                while offset < chunk.len() {
                    let written = producer
                        .push_interleaved(&chunk[offset..])
                        .unwrap()
                        .frames();
                    if written == 0 {
                        thread::yield_now();
                    } else {
                        offset += written * producer.channels();
                    }
                }
            }
        });
        let reader = thread::spawn(move || {
            let mut received = Vec::with_capacity(expected_len);
            let mut block = [0.0; 20];
            while received.len() < expected_len {
                let read = consumer.pop_interleaved(&mut block).unwrap().frames();
                if read == 0 {
                    thread::yield_now();
                } else {
                    received.extend_from_slice(&block[..read * consumer.channels()]);
                }
            }
            received
        });
        writer.join().unwrap();
        let received = reader.join().unwrap();
        assert_eq!(received, expected);
    }
}
