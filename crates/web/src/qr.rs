//! 本地生成二维码 SVG，不访问任何外部服务。

use qrcode::render::svg;
use qrcode::QrCode;

/// 把文本编码成二维码 SVG（白底、含留白，约 220px）；内容过长或编码失败返回 None。
pub(crate) fn qr_svg(text: &str) -> Option<String> {
    let code = QrCode::new(text.as_bytes()).ok()?;
    Some(
        code.render::<svg::Color>()
            .min_dimensions(220, 220)
            .max_dimensions(220, 220)
            .quiet_zone(true)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_svg() {
        let s = qr_svg("https://liteapp.weixin.qq.com/q/abc?qrcode=1234&bot_type=3").unwrap();
        assert!(s.contains("<svg") && s.contains("#ffffff"));
    }
}
