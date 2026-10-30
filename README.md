# COS Fusion — Windows Preview v0.2.2

Projek baharu yang berasingan daripada Chat On Steroids 2.1.42 / 2.1.46. Sekarang ada **agent runtime dan GUI web localhost yang boleh dijalankan**. GUI dibuka dalam browser biasa; ini belum aplikasi native Electron, produk penuh atau cloud. Tidak mengubah COS sedia ada.

## Windows portable preview

`npm run dist:win` menghasilkan `release/COS-Fusion-0.2.2-preview-Windows-x64.zip`, yang mengandungi EXE standalone, arahan penggunaan dan SHA256. **Tester Windows x64 tidak perlu memasang Node.js**. Preview masih unsigned dan untuk ujian terhad, bukan release awam yang diperakui.

Double-click `COS-Fusion-Preview.exe` selepas extract. Browser membuka UI COS Fusion; tutup console yang kekal terbuka untuk tamatkan proses. Dalam **Customize**, hidupkan modul Workspace, kemudian cuba `/list` dengan provider Demo. Modul Workspace sengaja OFF pada permulaan bagi melindungi fail. UI Fusion Workspace menggunakan navigasi yang boleh dibuka pada paparan kecil, shortcut `Ctrl+K` untuk chat baharu, loading state dan status modul.

## Jalankan daripada source

Keperluan: Node.js 22 ke atas. Tiada `npm install` diperlukan.

```powershell
node .\bin\cos.mjs status
node .\bin\preview.mjs
node .\bin\cos.mjs demo /list
node .\bin\cos.mjs modules
node .\bin\cos.mjs module enable notes
node .\bin\cos.mjs skill add .\examples\review-skill.md
node .\bin\cos.mjs skill enable review-skill
npm test
npm run qa:ui
```

Semua keadaan demo berada di `~/.cos-fusion` secara lalai. Tetapkan `COS_FUSION_HOME` untuk menggunakan folder ujian sendiri. Tiada proses yang terus berjalan selepas arahan tamat.

## Cuba model sebenar

```powershell
$env:OPENAI_API_KEY = '<your-key>'
node .\bin\cos.mjs ask openai <model-id> 'List project files'

$env:ANTHROPIC_API_KEY = '<your-key>'
node .\bin\cos.mjs ask anthropic <model-id> 'Explain this project'

$env:OPENROUTER_API_KEY = '<your-key>'
node .\bin\cos.mjs ask openrouter <model-id> 'Review README'
```

Ollama juga boleh dipilih (`ask ollama <model-id> ...`) jika Ollama berjalan dalam mesin sendiri. `COS_FUSION_ENDPOINT` boleh menukar URL API untuk adapter semasa; API mesti melalui HTTPS kecuali loopback tempatan. Keys hanya dibaca dari environment, tidak disimpan oleh COS Fusion. Model dan akaun perlu tersedia di provider masing-masing. Pengujian dengan model sebenar memerlukan credentials pengguna dan tidak dibuat secara automatik.

## Apa yang benar-benar berfungsi

- Agent loop bebas model dengan sokongan panggilan tool dan hasil yang disemak.
- Adapter OpenAI-compatible (OpenAI, OpenRouter, Ollama) dan Anthropic Messages.
- Hermes Agent CLI optional: pilih Hermes dalam UI, hidupkan toggle External agents di Customize, kemudian hantar prompt. Hermes menggunakan pemasangan, konfigurasi model dan tools sendiri. Pilihan model boleh dikosongkan. Sesi Hermes disambung apabila thread yang sama digunakan. COS tidak memasang Hermes secara automatik.
- Modul `workspace` dan `notes` dengan enable/disable dan semakan kebenaran semasa tool digunakan.
- Tambah, hidupkan, matikan dan buang fail Skill Markdown yang disalin ke direktori aplikasi.
- Demo provider tanpa kunci, rekod sehingga 25 hasil sesi setempat, ujian automatik.
- GUI web localhost dengan thread sambung sembang, pemilih provider, API key dalam memori sementara, modules & skills management.
- ZIP portable dengan EXE Windows x64 menggunakan Node single executable applications (SEA).

## Belum diimplementasi

GUI native/Electron, Codex app-server dan Claude Code CLI integration, Hermes agent gateway penuh, subagents, browser live preview, desktop input, cloud agent runner, job queues, automatic updates, encrypted local storage, sandbox sebenar untuk third-party executable plugins dan backup/migrasi data COS lama.

**Maklumat privasi:** prototaip menyimpan prompt/jawapan dan sehingga 25 thread dalam `state.json` tempatan sebagai teks biasa. Gunakan workspace dan prompt ujian; ia belum sesuai untuk data sulit. Credentials yang ditaip dalam GUI tidak disimpan pada disk. Model sebenar memerlukan API provider berasingan dan mungkin berbayar. Hermes menggunakan akaun, konfigurasi, tool permissions dan penyimpanan sesi milik Hermes sendiri; ia mungkin menjalankan arahan atau mengubah fail jika konfigurasi Hermes membenarkannya. Hidupkan External agents hanya apabila mahu menggunakan agent tersebut.

Lihat [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PRODUCT-ROADMAP.md](docs/PRODUCT-ROADMAP.md), dan [docs/BETA-README.txt](docs/BETA-README.txt) untuk binaan penuh serta had beta.
