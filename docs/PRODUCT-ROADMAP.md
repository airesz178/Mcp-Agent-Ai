# Mcp Agent Ai — arah produk untuk pengguna awam

**Platform rasmi disasarkan sejak 2026-11-04:** hanya **Android 8.0+ (API 26), ARM64**. Windows 10/11 kini komputer pembangunan sahaja, bukan lagi produk atau sasaran installer. Linux, macOS dan iOS tidak disokong. Android menggunakan adapter native, GeckoView dan kawalan kebenaran Android. Jangan mendakwa parity penuh tanpa ujian peranti sebenar.

**Visi:** Sebuah aplikasi AI agent yang modular dan mesra pengguna, menyatukan workflow coding, research, browser, automation, skills dan banyak model/provider dalam satu tempat. Produk ini perlu dinilai melalui ujian kegunaan, ketahanan dan prestasi, bukan dakwaan awal bahawa ia mengatasi semua pesaing.

**Pemisahan execution:** komputer pengguna untuk fail/desktop semasa hidup, cloud runner pilihan untuk tugasan yang perlu berterusan ketika komputer dimatikan. Perkhidmatan cloud tetap memerlukan compute, identiti dan kos. Tiada laptop wajib berfungsi sebagai VPS.

**Pengalaman pengguna sasaran:** APK Android bertandatangan yang dipercayai, perbualan berterusan, tugas berbilang agent yang jelas statusnya, file tree, diff sebelum apply, terminal, GeckoView, plugin/skills galeri, monitor job, kawalan belanja model dan pengurusan data.

**Modular:** tambah/buang skill, provider dan tool berdasarkan manifest versi; kebenaran live sebelum tool action; plugin executable perlu proses terasing dan pembatalan. Port komponen daripada COS 2.1.46 hanya selepas ujian kontrak dan lesen.

**Proses beta awam:** bina satu APK Android, privacy/security review, signing release Android, ujian upgrade in-place/data migration, lifecycle, Play Protect/permission, jangkaan kos API; host APK dan SHA256 dari sumber rasmi; release notes serta issue tracker. Tanda awal v0.2 sebagai *preview for testers*, bukan stable.

**Status semasa:** GUI local-first, basic conversations dan read-only workspace tool, module/skill toggle, per-process API keys, adapter API serta Hermes CLI one-shot JSONL dengan izin eksplisit. Ini langkah demonstrasi awal sahaja; native desktop, coding agent penuh, cloud jobs, Codex/Claude Code dan Hermes gateway lanjutan belum lengkap.
