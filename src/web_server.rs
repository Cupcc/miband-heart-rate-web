use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock, Mutex};
use tokio::time::{interval, Duration};
use warp::Filter;
use warp::ws::{WebSocket, Ws, Message};
use chrono::Local;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartRateData {
    pub timestamp: String,
    pub heart_rate: u16,
    pub sensor_contact: Option<bool>,
    pub device_connected: bool,  // 添加设备连接状态
}

pub type Clients = Arc<RwLock<HashMap<usize, tokio::sync::mpsc::UnboundedSender<Message>>>>;
pub type HeartRateSender = broadcast::Sender<HeartRateData>;

// 心率数据缓存结构
#[derive(Debug, Clone)]
pub struct HeartRateBuffer {
    pub current_heart_rate: Option<u16>,
    pub sensor_contact: Option<bool>,
    pub last_update: SystemTime,
    pub device_connected: bool,  // 添加设备连接状态
}

impl Default for HeartRateBuffer {
    fn default() -> Self {
        Self {
            current_heart_rate: None,
            sensor_contact: None,
            last_update: UNIX_EPOCH,
            device_connected: false,  // 默认为未连接状态
        }
    }
}

pub async fn start_web_server(heart_rate_sender: HeartRateSender) -> Result<(), Box<dyn std::error::Error>> {
    let clients: Clients = Arc::new(RwLock::new(HashMap::new()));
    let clients_clone = clients.clone();
    
    // 创建心率数据缓存
    let heart_rate_buffer = Arc::new(Mutex::new(HeartRateBuffer::default()));
    let buffer_for_receiver = heart_rate_buffer.clone();
    let buffer_for_timer = heart_rate_buffer.clone();
    
    // 启动心率数据接收任务（更新缓存）
    let heart_rate_receiver = heart_rate_sender.subscribe();
    tokio::spawn(update_heart_rate_buffer(buffer_for_receiver, heart_rate_receiver));
    
    // 启动定时广播任务（每秒发送一次）
    tokio::spawn(timed_broadcast_heart_rate(clients_clone, buffer_for_timer));

    // CORS配置，允许前端跨域访问
    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type", "authorization", "accept"])
        .allow_methods(vec!["GET", "POST", "DELETE", "PUT"]);

    // WebSocket路由
    let clients_for_ws = clients.clone();
    let websocket = warp::path("ws")
        .and(warp::ws())
        .map(move |ws: Ws| {
            let clients = clients_for_ws.clone();
            ws.on_upgrade(move |socket| handle_websocket(socket, clients))
        });

    // 健康检查API
    let health_check = warp::path("api")
        .and(warp::path("health"))
        .and(warp::path::end())
        .map(|| {
            warp::reply::json(&serde_json::json!({
                "status": "healthy",
                "service": "miband-heart-rate-api",
                "timestamp": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
            }))
        });

    // 获取当前心率数据API
    let buffer_for_api = heart_rate_buffer.clone();
    let heartrate_api = warp::path("api")
        .and(warp::path("heartrate"))
        .and(warp::path::end())
        .and_then(move || {
            let buffer = buffer_for_api.clone();
            async move {
                let buffer_guard = buffer.lock().await;
                let heart_rate_data = HeartRateData {
                    timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    heart_rate: buffer_guard.current_heart_rate.unwrap_or(0),
                    sensor_contact: buffer_guard.sensor_contact,
                    device_connected: buffer_guard.device_connected,
                };
                Ok::<_, warp::Rejection>(warp::reply::json(&heart_rate_data))
            }
        });

    let routes = websocket
        .or(health_check)
        .or(heartrate_api)
        .with(cors);

    println!("心率监控后端API服务器启动在 http://localhost:3030");
    println!("可用的API端点:");
    println!("  - WebSocket连接: ws://localhost:3030/ws (实时心率数据)");
    println!("  - 健康检查: http://localhost:3030/api/health");
    println!("  - 当前心率: http://localhost:3030/api/heartrate");
    
    warp::serve(routes)
        .run(([127, 0, 0, 1], 3030))
        .await;

    Ok(())
}

async fn handle_websocket(ws: WebSocket, clients: Clients) {
    static NEXT_CLIENT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    let client_id = NEXT_CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    
    let (mut ws_sender, mut ws_receiver) = ws.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    
    // 将客户端添加到连接列表
    clients.write().await.insert(client_id, tx);
    println!("客户端 {} 已连接", client_id);

    // 处理发送消息的任务
    let send_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if ws_sender.send(message).await.is_err() {
                break;
            }
        }
    });

    // 处理接收消息的任务（保持连接活跃）
    let recv_task = tokio::spawn(async move {
        while let Some(result) = ws_receiver.next().await {
            match result {
                Ok(msg) => {
                    if msg.is_close() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // 等待任一任务完成
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    // 清理客户端连接
    clients.write().await.remove(&client_id);
    println!("客户端 {} 已断开连接", client_id);
}

// 更新心率数据缓存
async fn update_heart_rate_buffer(
    buffer: Arc<Mutex<HeartRateBuffer>>,
    mut heart_rate_receiver: broadcast::Receiver<HeartRateData>,
) {
    while let Ok(heart_rate_data) = heart_rate_receiver.recv().await {
        let mut buffer_guard = buffer.lock().await;
        buffer_guard.current_heart_rate = Some(heart_rate_data.heart_rate);
        buffer_guard.sensor_contact = heart_rate_data.sensor_contact;
        buffer_guard.device_connected = heart_rate_data.device_connected;
        buffer_guard.last_update = SystemTime::now();
        drop(buffer_guard);
        
        let connection_status = if heart_rate_data.device_connected { "已连接" } else { "已断开" };
        println!("缓存已更新: 心率 {} BPM，设备状态: {}", heart_rate_data.heart_rate, connection_status);
    }
}

// 定时广播心率数据（每秒一次）
async fn timed_broadcast_heart_rate(
    clients: Clients,
    buffer: Arc<Mutex<HeartRateBuffer>>,
) {
    let mut interval_timer = interval(Duration::from_secs(1));
    
    loop {
        interval_timer.tick().await;
        
        let mut buffer_guard = buffer.lock().await;
        
        // 检查是否有有效的心率数据
        if let Some(heart_rate) = buffer_guard.current_heart_rate {
            // 检查数据是否太旧（超过10秒没有更新）
            let elapsed = SystemTime::now()
                .duration_since(buffer_guard.last_update)
                .unwrap_or(Duration::from_secs(0));
            
            if elapsed > Duration::from_secs(10) {
                // 数据过时，标记设备为断开连接状态
                buffer_guard.device_connected = false;
                println!("心率数据过时（超过10秒），设备可能已断开连接");
                
                // 发送设备断开连接的状态信息
                let disconnected_data = HeartRateData {
                    timestamp: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    heart_rate: 0,  // 断开连接时心率显示为0
                    sensor_contact: None,
                    device_connected: false,
                };
                
                drop(buffer_guard);
                broadcast_to_clients(&clients, &disconnected_data).await;
                continue;
            }
            
            let heart_rate_data = HeartRateData {
                timestamp: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                heart_rate,
                sensor_contact: buffer_guard.sensor_contact,
                device_connected: buffer_guard.device_connected,
            };
            
            drop(buffer_guard);
            broadcast_to_clients(&clients, &heart_rate_data).await;
            
            let connection_status = if heart_rate_data.device_connected { "已连接" } else { "连接中断" };
            println!("定时广播心率数据: {} BPM, 设备状态: {}", heart_rate, connection_status);
        } else {
            drop(buffer_guard);
            
            // 发送无数据状态
            let no_data = HeartRateData {
                timestamp: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                heart_rate: 0,
                sensor_contact: None,
                device_connected: false,
            };
            
            broadcast_to_clients(&clients, &no_data).await;
            println!("暂无心率数据可广播，设备未连接");
        }
    }
}

// 辅助函数：向所有客户端广播数据
async fn broadcast_to_clients(clients: &Clients, heart_rate_data: &HeartRateData) {
    let message = match serde_json::to_string(heart_rate_data) {
        Ok(json) => Message::text(json),
        Err(e) => {
            eprintln!("序列化心率数据失败: {}", e);
            return;
        }
    };

    let clients_guard = clients.read().await;
    let mut disconnected_clients = Vec::new();

    for (&client_id, sender) in clients_guard.iter() {
        if sender.send(message.clone()).is_err() {
            disconnected_clients.push(client_id);
        }
    }
    
    drop(clients_guard);

    // 清理断开连接的客户端
    if !disconnected_clients.is_empty() {
        let mut clients_guard = clients.write().await;
        for client_id in disconnected_clients {
            clients_guard.remove(&client_id);
        }
    }
}
