<p align="center">
  <img src="assets/icon.svg" alt="Nicord" width="128" height="128">
</p>

<h1 align="center">Nicord</h1>

<p align="center">
  Ứng dụng máy tính gửi tin nhắn Discord tự động, theo lịch bạn đặt — bằng chính tài khoản của bạn.
</p>

<p align="center">
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

Bạn chỉ cần mở app, soạn nội dung, đặt lịch, rồi để app chạy. Ở chế độ Tài khoản, tin nhắn gửi đi hiện như chính bạn gửi — không cần mời bot vào server, không cần lập trình. Ở chế độ Webhook (khuyến nghị, cách chính thức của Discord), tin nhắn hiện dưới tên webhook của kênh.

> ⚠️ **ĐỌC TRƯỚC KHI DÙNG — RẤT QUAN TRỌNG**
>
> Discord **cấm** việc tự động hoá tài khoản người dùng thường (gọi là "self-bot"). Nếu bị Discord phát hiện, tài khoản của bạn **có thể bị khoá vĩnh viễn**.
>
> Nicord có nhiều biện pháp để **giảm** rủi ro (gửi chậm, giãn cách, xoay nội dung...), nhưng **không thể hứa là bạn sẽ không bao giờ bị khoá**.
>
> 👉 **Hãy dùng một tài khoản phụ để thử. Đừng dùng tài khoản chính** có server, bạn bè hay Nitro quan trọng.
> Bạn tự chịu trách nhiệm với tài khoản của mình khi dùng Nicord.
>
> Muốn an toàn hơn? Dùng **chế độ Webhook** bên dưới — đó là cách chính thức của Discord, không dùng tài khoản của bạn để gửi.

---

## Chế độ Webhook (khuyến nghị)

Webhook là cách chính thức của Discord để gửi tin nhắn tự động tới 1 kênh. Bạn không cần token tài khoản, không cần mời bot, và không có nguy cơ bị khóa tài khoản vì gửi tin.

**Cách tạo webhook:**

1. Trong Discord, bấm chuột phải vào kênh bạn muốn gửi → **Edit Channel** → **Integrations** → **Webhooks** → **New Webhook**.
2. Đặt tên cho webhook (tên này sẽ hiện khi tin nhắn được gửi), rồi bấm **Copy Webhook URL**.
3. Mở Nicord, chọn **Webhook** ở màn hình đầu tiên, dán URL vào, bấm **Kiểm tra webhook** để xem đúng kênh chưa, rồi **Lưu webhook**.

**Lưu ý:**

- URL webhook **là mật khẩu** — ai có URL đều gửi được tin vào kênh đó. Nicord mã hóa URL và chỉ lưu trên máy bạn, không bao giờ hiện đủ URL hay ghi vào log. Nếu URL bị lộ, hãy xóa webhook cũ và tạo webhook mới trong Discord.
- Webhook **chỉ gửi tin nhắn**. Webhook không gửi được lệnh (prefix/slash), không hiện "đang gõ...", không tự xóa tin. Các kịch bản có lệnh sẽ bị từ chối ở chế độ Webhook.
- Mỗi webhook gắn với đúng 1 kênh. Muốn đổi kênh, tạo webhook mới ở kênh đó rồi đổi URL trong Cài đặt.
- Bạn có thể đổi qua lại giữa Webhook và Tài khoản trong Cài đặt — thông tin đã lưu của chế độ kia không bị xóa.

**Tiện ích khi soạn và theo dõi:**

- Xem trước tin nhắn kiểu Discord ngay khi gõ, kèm đếm ký tự 2000; tin quá dài sẽ bị chặn khi thêm/bật.
- Trạng thái webhook (tốt / không tìm thấy / lỗi mạng) và thời điểm kiểm tra gần nhất hiện ở Dashboard và Cài đặt; webhook bị xóa sẽ tự tạm dừng gửi cho tới khi kiểm tra lại.
- Import kịch bản có cảnh báo rõ ràng (thiếu trường, file lỗi, file chứa URL webhook).
- Logs lọc theo từ khóa, trạng thái và kịch bản; xuất CSV không bao giờ chứa URL/token.
- Nhân bản kịch bản và đặt tên thân thiện cho webhook.

---

## Bắt đầu trong 5 phút (chế độ Tài khoản)

### Bước 1 — Nhập token Discord của bạn

Token giống như "mật khẩu riêng" để app gửi tin thay bạn. Bạn tự lấy token của chính mình trong Discord (như cách một lập trình viên lấy token vậy), rồi dán vào app **một lần duy nhất**.

- Nicord **mã hoá token và chỉ lưu trên máy của bạn**, không gửi token đi đâu cả.
- **Không bao giờ chia sẻ token cho bất kỳ ai**, kể cả người tự xưng là hỗ trợ Nicord. Nếu token bị lộ, hãy đổi token ngay trong Discord.

### Bước 2 — Chọn server và channel

Bấm **Load servers**, chọn server rồi chọn channel bạn muốn gửi tin vào. Bạn cũng có thể dán mã số của kênh bằng tay — trong Discord, bấm chuột phải vào tên kênh rồi chọn Copy Channel ID.

### Bước 3 — Soạn kịch bản

Một kịch bản gồm: **nội dung + lịch gửi**.

- Soạn nhiều mẫu tin, app sẽ tự xoay vòng.
- Dùng **spin text** để mỗi lần gửi khác nhau một chút, ví dụ: `{Chào buổi sáng|Hello|Hi} cả nhà!`
- Dùng **biến** để tin nhắn tự điền thông tin, ví dụ: `{{date}}` thành ngày hôm nay, `{{time}}` thành giờ hiện tại.
- Đặt lịch: gửi một lần, lặp mỗi 30 phút, mỗi giờ, mỗi ngày, mỗi tuần, hoặc lịch nâng cao (cron).

### Bước 4 — Gửi thử rồi bật chạy

- Bấm **gửi thử 1 tin** để chắc chắn mọi thứ đúng, rồi mới bật chạy thật.
- Bấm **Start engine** để app bắt đầu chạy theo lịch. App sẽ tự chạy lại các kịch bản đang bật mỗi khi bạn mở app lên.
- Giữ cửa sổ app mở (bấm thu nhỏ cũng được). **Đóng cửa sổ thì app sẽ dừng gửi.**

---

## Giới hạn

Nicord là một sản phẩm duy nhất, đơn giản cho mọi người dùng:

- **1 channel** duy nhất để gửi tin.
- Có **giới hạn gửi mỗi ngày** để giảm rủi ro bị khoá tài khoản. Hết giới hạn thì app tự dừng và gửi tiếp vào ngày hôm sau.

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
Trên Windows thì có: bật "Thu nhỏ xuống khay hệ thống" trong Cài đặt, đóng cửa sổ sẽ ẩn app xuống tray thay vì thoát. Trên Linux hiện chưa có tray, bạn cần giữ cửa sổ app mở (có thể thu nhỏ). Tự chạy cùng máy tính đã có trên cả Windows và Linux.

**App chạy trên máy nào?**
App viết để chạy trên Windows, macOS và Linux, nhưng hiện mới chỉ chạy thử trên Linux. App rất nhẹ, mở lên gần như ngay lập tức.

---

## Tải về và cài đặt

Hiện tại chưa có file cài đặt sẵn. Để chạy Nicord, bạn cần tự build từ mã nguồn — hướng dẫn từng bước (dành cho người quen với lập trình) có trong [docs/TECHNICAL.md](docs/TECHNICAL.md).

---

## Giới hạn đã biết

Nói thẳng để bạn không bất ngờ:

- **Icon khay hệ thống chỉ có trên Windows.** Trên Linux, bạn cần mở app và giữ cửa sổ mở để app chạy. Tự chạy cùng máy tính đã có trên cả Windows và Linux.
- **Chưa thử nghiệm gửi ổn định 7 ngày liên tục.** Trước khi dùng lâu dài, bạn nên tự chạy thử trên tài khoản phụ vài ngày.
- App chỉ gửi tin theo lịch, không đọc tin nhắn trong server.

---

## Dành cho lập trình viên

Mọi chi tiết kỹ thuật — cách build, cấu trúc code, cách hoạt động của lịch gửi và chống ban — đều nằm trong thư mục [`docs/`](docs/):

- [docs/TECHNICAL.md](docs/TECHNICAL.md) — build, cài đặt, chống ban, lịch gửi
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — kiến trúc, luồng dữ liệu, từng file code làm gì
- [examples/scenario-example.json](examples/scenario-example.json) — ví dụ một kịch bản mẫu

Giấy phép: MIT.

---

## English summary

Nicord is a lightweight desktop app that sends scheduled messages to a Discord server channel using your own account. Set a message and a schedule, and it sends for you. Automating a normal Discord account ("self-bot") violates Discord's Terms of Service and can get your account permanently banned — use a spare account. One simple app for everyone: 1 channel, with a daily sending limit to protect your account. See [docs/TECHNICAL.md](docs/TECHNICAL.md) for technical details.
