// config_private.rs —— 私密配置（WiFi密码、服务器IP等）
// 复制此文件为config_private.rs，填入你自己的配置

/// WiFi名称
pub const WIFI_SSID: &str = "你的WiFi名称";

/// WiFi密码
pub const WIFI_PASSWORD: &str = "你的WiFi密码";

/// 服务端IP地址（运行talk-with-anyone服务端的电脑IP）
pub const SERVER_HOST: &str = "192.168.1.100";

/// 服务端端口
pub const SERVER_PORT: u16 = 7862;

/// 访问令牌（如果服务端启用了token验证，留空表示不使用）
pub const ACCESS_TOKEN: &str = "";
