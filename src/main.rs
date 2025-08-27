use std::error::Error;

use bluest::{btuuid::bluetooth_uuid_from_u16, Adapter, Device, Uuid};
use futures_lite::stream::StreamExt;
use chrono::Local;
use tokio::sync::broadcast;

mod web_server;
use web_server::{HeartRateData, start_web_server};

const HRS_UUID: Uuid = bluetooth_uuid_from_u16(0x180D);
const HRM_UUID: Uuid = bluetooth_uuid_from_u16(0x2A37);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // 创建心率数据广播通道
    let (heart_rate_sender, _) = broadcast::channel::<HeartRateData>(100);
    let heart_rate_sender_clone = heart_rate_sender.clone();

    // 启动蓝牙心率监控（在后台运行，不阻塞Web服务器）
    let bluetooth_sender = heart_rate_sender.clone();
    tokio::spawn(async move {
        loop {
                    match run_bluetooth_monitor(bluetooth_sender.clone()).await {
            Ok(_) => println!("蓝牙监控正常结束"),
            Err(e) => {
                eprintln!("蓝牙监控失败: {}", e);
                
                // 发送设备断开连接的状态信息
                let _ = bluetooth_sender.send(HeartRateData {
                    timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    heart_rate: 0,
                    sensor_contact: None,
                    device_connected: false,
                });
                
                println!("5秒后重试蓝牙连接...");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
        }
    });

    // 启动Web服务器（主要任务）
    println!("正在启动Web服务器...");
    if let Err(e) = start_web_server(heart_rate_sender_clone).await {
        eprintln!("Web服务器启动失败: {}", e);
    }

    Ok(())
}

async fn run_bluetooth_monitor(heart_rate_sender: broadcast::Sender<HeartRateData>) -> Result<(), Box<dyn Error + Send + Sync>> {
    let adapter = Adapter::default()
        .await
        .ok_or("Bluetooth adapter not found")?;
    adapter.wait_available().await?;

    loop {
        let device = {
            let connected_heart_rate_devices =
                adapter.connected_devices_with_services(&[HRS_UUID]).await?;
            if let Some(device) = connected_heart_rate_devices.into_iter().next() {
                device
            } else {
                println!("正在扫描心率设备...");
                let mut scan = adapter.discover_devices(&[HRS_UUID]).await?;

                println!("扫描已启动");
                let device = scan.next().await.ok_or("未找到设备")??;

                println!("找到设备: [{}] {:?}", device, device.name_async().await);
                device
            }
        };

        if let Err(err) = handle_device(&adapter, &device, &heart_rate_sender).await {
            println!("连接错误: {err:?}");
            // 等待5秒后重试
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    }
}

async fn handle_device(
    adapter: &Adapter, 
    device: &Device,
    heart_rate_sender: &broadcast::Sender<HeartRateData>
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Connect
    if !device.is_connected().await {
        println!("正在连接设备: {}", device.id());
        adapter.connect_device(&device).await?;
    }

    // Discover services
    let heart_rate_services = device.discover_services_with_uuid(HRS_UUID).await?;
    let heart_rate_service = heart_rate_services
        .first()
        .ok_or("设备应该至少有一个心率服务")?;

    // Discover characteristics
    let heart_rate_measurements = heart_rate_service
        .discover_characteristics_with_uuid(HRM_UUID)
        .await?;
    let heart_rate_measurement = heart_rate_measurements
        .first()
        .ok_or("心率服务应该至少有一个心率测量特征")?;

    let mut updates = heart_rate_measurement.notify().await?;
    println!("开始接收心率数据...");
    
    while let Some(Ok(heart_rate)) = updates.next().await {
        // 获取当前时间
        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        
        // 解析心率数据
        let flag = *heart_rate.get(0).ok_or("没有标志位")?;

        // Heart Rate Value Format
        let mut heart_rate_value = *heart_rate.get(1).ok_or("没有心率数据")? as u16;
        if flag & 0b00001 != 0 {
            heart_rate_value |= (*heart_rate.get(2).ok_or("没有心率高位数据")? as u16) << 8;
        }

        // Sensor Contact Supported
        let mut sensor_contact = None;
        if flag & 0b00100 != 0 {
            sensor_contact = Some(flag & 0b00010 != 0)
        }
        
        // 创建心率数据结构
        let heart_rate_data = HeartRateData {
            timestamp: now.clone(),
            heart_rate: heart_rate_value,
            sensor_contact,
            device_connected: true,  // 收到数据说明设备已连接
        };
        
        // 发送数据到Web界面
        if let Err(_) = heart_rate_sender.send(heart_rate_data) {
            println!("警告: 没有Web客户端连接");
        }
        
        // 控制台输出
        println!("[{now}] 心率: {heart_rate_value} BPM, 传感器接触: {sensor_contact:?}");
    }
    
    Err("心率通知已停止".into())
}
