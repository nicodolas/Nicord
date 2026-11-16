# Nicord

Desktop Discord auto-sender — gửi tin nhắn tự động vào channel trong server bằng **chính tài khoản của bạn**, theo lịch và kịch bản bạn tự dựng.

> ⚠️ **CẢNH BÁO ĐỎ — ĐỌC TRƯỚC KHI DÙNG**
>
> Tự động hoá tài khoản người dùng Discord ("self-bot") **bị cấm bởi Điều khoản Dịch vụ của Discord** và **có thể dẫn đến khoá tài khoản vĩnh viễn** khi bị phát hiện.
> Nicord **không hứa "không bao giờ bị ban"**. Mọi biện pháp chống ban trong app chỉ *giảm* rủi ro, không xoá được rủi ro.
> **Hãy dùng tài khoản phụ để thử nghiệm. Đừng dùng tài khoản chính có server, Nitro hay bạn bè quan trọng.**
> Bạn tự chịu trách nhiệm với tài khoản của mình khi dùng Nicord.

> ⚠️ **RED WARNING — READ FIRST**
>
> Automating a normal Discord user account ("self-bot") is **forbidden by Discord's Terms of Service** and **can result in permanent account termination**.
> Nicord makes **no guarantee you will never be banned**. Anti-ban measures only *reduce* risk.
> **Use a spare account. Never your main account.** You are responsible for your own account.

## Nicord là gì?

- Desktop app, local-first, **không có đăng nhập riêng**. Mở app, nhập key một lần để mở tính năng, nhập token Discord của bạn một lần, chọn server + channel, dựng kịch bản, bật chạy nền (giữ cửa sổ mở — thu nhỏ được; **chưa có icon system tray**, đóng cửa sổ sẽ dừng engine, xem mục "Giới hạn đã biết" bên dưới).
- **Không phải Bot API bot.** Tin nhắn gửi đi hiện như chính bạn gửi. Không cần mời bot vào server.
- Kịch bản kéo-thả / chọn node: Trigger → Condition → Action → Utility, có sẵn mẫu.
- Lịch gửi: một lần, lặp theo phút/giờ/ngày, hàng tuần, cron nâng cao (Pro).
- Free: **1 channel, 1 kịch bản đang chạy, 100 tin/ngày**. Pro mở thêm channel, kịch bản, tài khoản.

## Nhanh, nhẹ — không React

Nicord viết bằng **Rust + eframe/egui** (native, immediate-mode), **không React, không Electron, không WebView**.
Một file binary duy nhất, khởi động tức thì, RAM chỉ vài chục MB. Giao diện đơn giản, ưu tiên tốc độ hơn đẹp.

## Cài đặt & chạy

```bash
# Yêu cầu: Rust stable (rustup)
cargo build --release
./target/release/nicord
```

Trên Windows: chạy `nicord.exe` sau khi build. App lưu config tại thư mục config của hệ điều hành (`%APPDATA%/nicord` trên Windows, `~/.config/nicord` trên Linux).

Trên Linux cần thêm thư viện hệ thống cho egui/winit:

```bash
sudo apt-get install -y libxcb1-dev libxkbcommon-dev libxkbcommon-x11-0 libgl1-mesa-dev libx11-dev
```

## Cách dùng (5 bước)

1. **Nhập key** — key do Nico cấp, nhập 1 lần để mở tính năng. Key chỉ mở khoá, không chứa tài khoản.
2. **Nhập token Discord của bạn** — bạn tự lấy token như developer, tự nhập 1 lần. Nicord mã hoá AES-256-GCM và lưu ở máy bạn, **không gửi token đi đâu**, không hiện lại toàn bộ token.
3. **Chọn server + channel** — bấm "Load servers" rồi chọn server và channel trong danh sách (hoặc nhập channel ID bằng tay).
4. **Dựng kịch bản** — chọn node, điền nội dung, đặt lịch, dùng biến `{{username}}`, `{{date}}` và spin text `{chào|hello|hi}`. Node nào chạy được: Trigger Schedule/Manual, Send Message, Send Embed, Time Window, Wait, Random Pick, Set Variable. Node khác (Reply, Loop, Call API, …) sẽ bị từ chối khi kiểm tra — app không giả vờ chạy chúng.
5. **Test & chạy** — gửi thử 1 tin (dry-run để kiểm tra trước), rồi bật engine. Engine tự chạy lại các kịch bản đang bật khi mở app. Giữ cửa sổ mở (bấm Minimise để thu nhỏ); đóng cửa sổ sẽ dừng engine.

## Lấy token Discord như thế nào?

Đây là việc của bạn, giống như khi làm dev: bạn tự lấy token tài khoản của mình từ Discord.
**Không bao giờ chia sẻ token cho bất kỳ ai — kể cả người tự xưng là hỗ trợ Nicord.**
Nếu token lộ, đổi token ngay trong Discord và nhập lại vào Nicord.

## License key — giới hạn trung thực

Key có dạng `base64(payload).base64(signature)`, ký bằng Ed25519, app kiểm tra offline bằng public key nhúng sẵn. Nhập 1 lần, ân hạn 7 ngày sau hết hạn.

### Cấp key cho người mua (Nico, offline)

```bash
# 1. Tạo cặp khoá MỘT LẦN trên máy không bao giờ đưa vào repo:
cargo run --bin nicord-keygen -- gen
#    In ra private key (cất offline, không commit) và public key.

# 2. Dán public key vào EMBEDDED_PUBLIC_KEY trong src/license.rs
#    (thay mảng [0u8; 32] placeholder). Chưa thay thì mọi key đều bị từ chối.

# 3. Ký key cho người mua (ngày hết hạn tuỳ chọn):
cargo run --bin nicord-keygen -- sign <PRIVATE_KEY_BASE64> pro 2027-01-01
```

**Giới hạn phải nói rõ:** mã nguồn mở + kiểm tra key phía client **không thể chống sao chép tuyệt đối** — người có kỹ thuật có thể biên dịch lại và bỏ qua bước kiểm tra. Vì vậy Nicord là open-core: giá trị trả phí nằm ở trải nghiệm mua hàng tốt, cập nhật, mẫu Pro và dịch vụ phía server trong tương lai (cloud runner, đồng bộ) — không phải chỉ ở bước kiểm tra key.

## Chống ban — 5 lớp (giảm rủi ro, không xoá rủi ro)

1. Giãn cách: tối thiểu 10 giây/tin/channel (giới hạn cứng không dưới 5 giây).
2. Jitter: +5–25 giây ngẫu nhiên mỗi lần gửi.
3. Rate limit: tôn trọng 429 và `retry_after`, không có tuỳ chọn tắt.
4. Nội dung: xoay vòng, spin text, chặn `@everyone`/`@here` mặc định.
5. Nhịp hoạt động: khung giờ nghỉ ban đêm, giới hạn tin/ngày, warm-up 3 ngày đầu ở 30%.

Không có tính năng vượt captcha, giả lập qua mặt phát hiện, hay xoay proxy hàng loạt.

## Scheduler — chạy đúng 1 lần mỗi lịch hẹn

Mỗi kịch bản ghi lại `last_run_at` (thời điểm theo lịch đã gửi gần nhất) vào SQLite sau mỗi lần gửi thành công. Engine chỉ gửi khi có một thời điểm theo lịch nằm trong khoảng (lần chạy trước, bây giờ] — nên EveryHours/EveryMinutes gửi đúng nhịp, Once chỉ gửi đúng 1 lần rồi tự tắt, Daily/Weekly không gửi trùng. Máy tắt thì không gửi bù dồn dập. Cron là parser 5 trường thật (phút giờ ngày tháng thứ), không còn coi cron là hourly.

## Cấu trúc project

```
src/
  main.rs       # entry point, eframe window
  app.rs        # egui UI: Onboarding, Dashboard, Builder, Logs, Settings
  scenario.rs   # scenario model, schedule + cron parser, spin-text, validation
  scheduler.rs  # send engine, per-channel queue, retry, 429 handling
  discord.rs    # Discord REST client (user token)
  license.rs    # Ed25519 license verification (offline)
  crypto.rs     # AES-256-GCM token encryption, machine-tied key
  store.rs      # SQLite (scenarios/logs/meta) + JSON config
  bin/nicord-keygen.rs  # tạo cặp khoá Ed25519 + ký license key (offline)
docs/ARCHITECTURE.md
examples/scenario-example.json
```

## Giới hạn đã biết (nói thẳng)

- **Chưa có icon system tray / autostart thật:** nút autostart trong Settings chỉ lưu lại, chưa gắn vào hệ điều hành. Giữ cửa sổ mở (thu nhỏ được) để engine chạy.
- **Spike kỹ thuật chưa chạy:** chưa chứng minh gửi ổn định 7 ngày liên tục trên tài khoản phụ — cần Nico chạy thử trước khi phát hành.
- **Channel picker** đã có (Load servers/channels), nhưng gửi tin vẫn qua REST, không dùng gateway.
- **Cron** đã là parser thật, nhưng múi giờ của lịch cron dùng múi giờ của kịch bản; kiểm tra kỹ khi đặt lịch qua nửa đêm.
- **Lịch `Once`** gửi đúng 1 lần rồi tự tắt kịch bản.

## Phát triển

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
cargo check
```

## Giấy phép

MIT — xem [LICENSE](LICENSE). Lõi mở, phần Pro theo giấy phép thương mại riêng khi phát hành.
