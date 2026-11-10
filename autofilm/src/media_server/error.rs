use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("媒体服务器 URL 无效: {0}")]
    InvalidUrl(String),

    #[error("媒体服务器 {id} 的 API Key 含有无法写入 MediaBrowser 认证头的字符")]
    InvalidApiKey { id: String },

    #[error("创建媒体服务器 HTTP 客户端失败: {0}")]
    BuildClient(#[source] reqwest::Error),

    #[error("媒体服务器请求失败: {0}")]
    Request(#[from] reqwest::Error),

    #[error("媒体服务器返回错误状态 {status}: {body}")]
    HttpStatus {
        status: reqwest::StatusCode,
        body: String,
    },

    #[error("媒体图片体积超过上限（{size} > {limit} 字节）")]
    ImageTooLarge { size: u64, limit: u64 },

    #[error("媒体服务器未返回可用用户")]
    MissingUser,
}
