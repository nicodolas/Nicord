<p align="center">
  <img src="assets/icon.svg" alt="Nicord" width="128" height="128">
</p>

<h1 align="center">Nicord</h1>

<p align="center">
  Ứng dụng máy tính gửi tin nhắn Discord tự động, theo lịch bạn đặt — bằng chính tài khoản của bạn.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="License: MIT">
  <img src="https://img.shields.io/badge/made%20with-Rust-orange" alt="Made with Rust">
  <img src="https://img.shields.io/badge/version-v0.1.0-blue" alt="Version v0.1.0">
</p>

---

## Nicord là gì?

Nicord giúp bạn **gửi tin nhắn vào một channel trong server Discord một cách tự động**, vào đúng giờ bạn hẹn, với nội dung bạn soạn sẵn.

Ví dụ bạn có thể dùng Nicord để:

- Chào buổi sáng cả server mỗi ngày lúc 8 giờ.
- Nhắc lịch sự kiện, lịch họp, lịch học mỗi tuần.
- Gửi thông báo, tin khuyến mãi, tin cập nhật theo giờ cố định.
- Xoay vòng nhiều mẫu tin khác nhau, để tin nhắn không bị lặp đi lặp lại giống hệt nhau.

Bạn chỉ cần mở app, soạn nội dung, đặt lịch, rồi để app chạy. **Tin nhắn gửi đi hiện như chính bạn gửi** — không cần mời bot vào server, không cần lập trình.

> ⚠️ **ĐỌC TRƯỚC KHI DÙNG — RẤT QUAN TRỌNG**
>
> Discord **cấm** việc tự động hoá tài khoản người dùng thường (gọi là "self-bot"). Nếu bị Discord phát hiện, tài khoản của bạn **có thể bị khoá vĩnh viễn**.
>
> Nicord có nhiều biện pháp để **giảm** rủi ro (gửi chậm, giãn cách, xoay nội dung...), nhưng **không thể hứa là bạn sẽ không bao giờ bị khoá**.
>
> 👉 **Hãy dùng một tài khoản phụ để thử. Đừng dùng tài khoản chính** có server, bạn bè hay Nitro quan trọng.
> Bạn tự chịu trách nhiệm với tài khoản của mình khi dùng Nicord.

---

## Bắt đầu trong 5 phút

### Bước 1 — Nhập key

Key là mã do Nico cấp để mở tính năng. Bạn chỉ cần nhập **một lần duy nhất**. Key không chứa tài khoản Discord của bạn.

### Bước 2 — Nhập token Discord của bạn

Token giống như "mật khẩu riêng" để app gửi tin thay bạn. Bạn tự lấy token của chính mình trong Discord (như cách một lập trình viên lấy token vậy), rồi dán vào app **một lần duy nhất**.

- Nicord **mã hoá token và chỉ lưu trên máy của bạn**, không gửi token đi đâu cả.
- **Không bao giờ chia sẻ token cho bất kỳ ai**, kể cả người tự xưng là hỗ trợ Nicord. Nếu token bị lộ, hãy đổi token ngay trong Discord.

### Bước 3 — Chọn server và channel

Bấm **Load servers**, chọn server rồi chọn channel bạn muốn gửi tin vào. Bạn cũng có thể dán mã số channel (channel ID) bằng tay.

### Bước 4 — Soạn kịch bản

Một kịch bản gồm: **nội dung + lịch gửi**.

- Soạn nhiều mẫu tin, app sẽ tự xoay vòng.
- Dùng **spin text** để mỗi lần gửi khác nhau một chút, ví dụ: `{Chào buổi sáng|Hello|Hi} cả nhà!`
- Dùng **biến** để tin nhắn tự điền thông tin, ví dụ: `{{date}}` thành ngày hôm nay, `{{time}}` thành giờ hiện tại.
- Đặt lịch: gửi một lần, lặp mỗi 30 phút, mỗi giờ, mỗi ngày, mỗi tuần, hoặc lịch nâng cao (Pro).

### Bước 5 — Gửi thử rồi bật chạy

- Bấm **gửi thử 1 tin** để chắc chắn mọi thứ đúng, rồi mới bật chạy thật.
- Bấm **Start engine** để app bắt đầu chạy theo lịch. App sẽ tự chạy lại các kịch bản đang bật mỗi khi bạn mở app lên.
- Giữ cửa sổ app mở (bấm thu nhỏ cũng được). **Đóng cửa sổ thì app sẽ dừng gửi.**

---

## Free và Pro

| | Free | Pro |
|---|---|---|
| Số channel gửi được | 1 | 20 |
| Số kịch bản chạy cùng lúc | 1 | 20 |
| Số tin nhắn mỗi ngày | 100 | 1.000 |
| Lịch nâng cao (cron) | Không | Có |

Bạn đang dùng Free hay Pro, app sẽ hiện rõ trên màn hình chính.

---

## Câu hỏi thường gặp

**Nicord có làm tài khoản của tôi bị khoá không?**
Có thể. Discord cấm tự động hoá tài khoản thường, và không có cách nào an toàn tuyệt đối. Nicord chỉ giúp giảm rủi ro bằng cách gửi chậm, giãn cách, xoay nội dung và nghỉ vào ban đêm. Hãy dùng tài khoản phụ.

**Nicord có lấy token hay tin nhắn của tôi không?**
Không. Mọi thứ lưu trên máy của bạn, token được mã hoá. App không gửi token hay nội dung của bạn đi đâu khác ngoài Discord.

**Tôi có cần mời bot vào server không?**
Không. Tin nhắn được gửi bằng chính tài khoản của bạn, nên không cần bot.

**Tắt máy thì tin nhắn có được gửi bù không?**
Không. Những lần hẹn trong lúc máy tắt sẽ bị bỏ qua, app chỉ gửi lần hẹn tiếp theo. App không gửi dồn dập để bù.

**App có chạy ẩn dưới khay hệ thống (tray) không?**
Chưa. Hiện tại bạn cần giữ cửa sổ app mở (có thể thu nhỏ). Tính năng chạy ẩn sẽ có trong bản sau.

**App chạy trên hệ điều hành nào?**
Windows, macOS và Linux. App rất nhẹ, mở lên gần như ngay lập tức.

---

## Tải về và cài đặt

Hiện tại chưa có bản tải sẵn (file cài đặt). Để chạy Nicord, bạn cần build từ mã nguồn — hướng dẫn từng bước có trong [docs/TECHNICAL.md](docs/TECHNICAL.md).

---

## Giới hạn đã biết

Nói thẳng để bạn không bất ngờ:

- **Chưa có icon khay hệ thống / tự chạy cùng máy tính.** Bạn cần mở app và giữ cửa sổ mở để app chạy.
- **Chưa thử nghiệm gửi ổn định 7 ngày liên tục.** Trước khi dùng lâu dài, bạn nên tự chạy thử trên tài khoản phụ vài ngày.
- Tin nhắn gửi qua kết nối thường (REST), không dùng kết nối trực tiếp (gateway) của Discord.

---

## Dành cho lập trình viên

Mọi chi tiết kỹ thuật — cách build, cấu trúc code, cách cấp license key, cách hoạt động của lịch gửi và chống ban — đều nằm trong thư mục [`docs/`](docs/):

- [docs/TECHNICAL.md](docs/TECHNICAL.md) — build, cài đặt, cấp key, chống ban, lịch gửi
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — kiến trúc, luồng dữ liệu, từng file code làm gì
- [examples/scenario-example.json](examples/scenario-example.json) — ví dụ một kịch bản mẫu

Giấy phép: MIT — xem [LICENSE](LICENSE).

---

## English summary

Nicord is a lightweight desktop app that sends scheduled messages to a Discord server channel using your own account. Set a message and a schedule, and it sends for you. Automating a normal Discord account ("self-bot") violates Discord's Terms of Service and can get your account permanently banned — use a spare account. Free plan: 1 channel, 1 scenario, 100 messages/day. See [docs/TECHNICAL.md](docs/TECHNICAL.md) for technical details.
