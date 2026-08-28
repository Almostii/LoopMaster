//! 常驻网络节点监听服务。
//!
//! 封装 [`MdnsBrowser`]，在后台持续轮询局域网 VBAN 节点上线/下线事件，
//! 维护当前节点列表快照，并把事件推送给订阅方（供 Tauri event 转发到前端）。
//!
//! 参考专项文档 6.3 节（服务浏览与拓扑节点动态注入）。

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::network::identity::{NodeIdentity, NodeMeta};
use crate::network::mdns::{MdnsAdvertiser, MdnsBrowser, MdnsError, NetworkEvent, NodeInfo};

/// 节点列表 + 订阅者（Arc 共享，供后台线程与外部同步访问）。
struct DiscoveryState {
    /// `node_id -> NodeInfo` 当前在线节点列表。
    nodes: HashMap<String, NodeInfo>,
    /// 事件订阅者（下线/下线广播目标）。
    subscribers: Vec<Sender<NetworkEvent>>,
}

/// 常驻网络节点监听服务。
///
/// - [`Self::start`] 在后台线程持续监听 mDNS 事件并维护节点列表；
/// - [`Self::snapshot`] 返回当前节点列表快照；
/// - 节点上线/下线通过 [`Self::subscribe`] 订阅（`NetworkEvent`）；
/// - 本机服务发布通过 [`Self::start_advertiser`] / [`Self::stop_advertiser`] 控制。
pub struct NetworkDiscovery {
    state: Arc<Mutex<DiscoveryState>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// 本机 mDNS 服务发布端（开启网络功能时注册，关闭时下架）。
    advertiser: Mutex<Option<MdnsAdvertiser>>,
}

impl NetworkDiscovery {
    /// 创建监听服务（未启动，调用 [`Self::start`] 后开始监听）。
    pub fn new() -> Result<Self, MdnsError> {
        Ok(Self {
            state: Arc::new(Mutex::new(DiscoveryState {
                nodes: HashMap::new(),
                subscribers: Vec::new(),
            })),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thread: None,
            advertiser: Mutex::new(None),
        })
    }

    /// 注册本机 VBAN 服务（发布 `_loopmaster-vban._udp.local.`）。
    ///
    /// 开启网络功能时调用；重复调用无操作。注册失败返回错误（不改变状态）。
    pub fn start_advertiser(
        &mut self,
        identity: &NodeIdentity,
        meta: &NodeMeta,
    ) -> Result<(), MdnsError> {
        let mut slot = self.advertiser.lock().expect("广告锁未中毒");
        if slot.is_some() {
            return Ok(());
        }
        let advertiser = MdnsAdvertiser::register(identity, meta)?;
        *slot = Some(advertiser);
        Ok(())
    }

    /// 下架本机 VBAN 服务（关闭网络功能时调用；幂等）。
    pub fn stop_advertiser(&mut self) {
        let mut slot = self.advertiser.lock().expect("广告锁未中毒");
        if let Some(advertiser) = slot.take() {
            drop(advertiser); // Drop 触发 unregister + daemon.shutdown
        }
    }

    /// 是否已发布本机服务（网络功能是否开启）。
    pub fn is_advertising(&self) -> bool {
        self.advertiser.lock().expect("广告锁未中毒").is_some()
    }

    /// 订阅节点上线/下线事件（多个订阅者会收到同一份事件）。
    ///
    /// 订阅可在 `start()` 之前或之后调用；后台线程每次广播时读取当前全部
    /// 订阅者，因此订阅者始终能收到后续事件。
    pub fn subscribe(&self) -> Receiver<NetworkEvent> {
        let (tx, rx) = mpsc::channel();
        self.state
            .lock()
            .expect("订阅锁未中毒")
            .subscribers
            .push(tx);
        rx
    }

    /// 启动后台监听线程。已在运行则无操作。
    pub fn start(&mut self) -> Result<(), MdnsError> {
        if self.thread.is_some() {
            return Ok(());
        }
        let state = Arc::clone(&self.state);
        let stop = Arc::clone(&self.stop);
        let mut browser = MdnsBrowser::new()?;
        let handle = thread::Builder::new()
            .name("loopmaster-mdns-discovery".into())
            .spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    // 用 try_recv + 短 sleep 轮询，保证能及时响应 stop 标志退出，
                    // 避免阻塞在 recv() 上导致 shutdown/join 死锁。
                    match browser.try_recv() {
                        Ok(Some(event)) => {
                            let mut state_guard = state.lock().expect("节点列表锁未中毒");
                            match &event {
                                NetworkEvent::NodeResolved(node) => {
                                    state_guard.nodes.insert(node.node_id.clone(), node.clone());
                                }
                                NetworkEvent::NodeRemoved(node_id) => {
                                    state_guard.nodes.remove(node_id);
                                }
                            }
                            // 读取当前订阅者快照后释放锁，再逐个发送。
                            let subscribers: Vec<Sender<NetworkEvent>> =
                                state_guard.subscribers.clone();
                            drop(state_guard);
                            for sub in &subscribers {
                                let _ = sub.send(event.clone());
                            }
                        }
                        Ok(None) => thread::sleep(Duration::from_millis(100)),
                        Err(MdnsError::ChannelClosed) => break,
                        Err(_) => thread::sleep(Duration::from_millis(100)),
                    }
                }
            })
            .expect("创建 mDNS 监听线程失败");
        self.thread = Some(handle);
        Ok(())
    }

    /// 停止后台监听线程并下架本机服务（幂等）。
    pub fn shutdown(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.stop_advertiser();
    }

    /// 当前在线节点列表快照（按 node_id 排序，保证确定性）。
    pub fn snapshot(&self) -> Vec<NodeInfo> {
        let state = self.state.lock().expect("节点列表锁未中毒");
        let mut list: Vec<NodeInfo> = state.nodes.values().cloned().collect();
        list.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        list
    }
}

impl Drop for NetworkDiscovery {
    fn drop(&mut self) {
        self.shutdown();
    }
}
