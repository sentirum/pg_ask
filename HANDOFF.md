# HANDOFF — pg_ask çalışma durumu

> Bu dosya bir oturum devri (session handoff) içindir. Yeni oturum bunu
> okuyup kaldığı yerden sorunsuz devam etmeli. Tarih: 2026-06-06.

## TL;DR — sıradaki iş

**RPM + APK + Docker image'i de Cloudsmith'e yayınla** ("tek source" hedefi).
Şu an sadece **APT** ve **Docker (GHCR)** var. RPM ve APK **sıfırdan** kurulacak.

---

## Bugün tamamlananlar (hepsi push'lu, pipeline yeşil)

1. **CI rustfmt fix** — 13 gündür (v0.5.3'ten) `cargo fmt --check` patlıyordu → `cargo fmt --all`.
2. **2 fixture test bug** — `tests/sql/01-fixture-baseline.sql` belirsiz `oid` → `pg_proc.oid`; `tests/run-fixture-tests.sh` yanlış port (15432→container socket) + `ON_ERROR_STOP=1` eksikti (hataları yutuyordu).
3. **Node 20 deprecation** — `actions/{checkout,cache,upload-artifact,download-artifact}` v4→v5; `softprops/action-gh-release` v2→v3.
4. **v0.5.6 release** — tag atıldı + public GitHub Release (tarball asset).
5. **APT SIGPIPE fix** — `build-deb.sh`'te `dpkg-deb -c | head` race → çıktıyı buffer'la.
6. **APT GPG fix** — `--import-ownertrust` 40-hane fingerprint ister; import edilen key'den türetiliyor.
7. **build-deb.sh upgrade-script** — hardcode liste → glob `sql/pg_ask--*--*.sql`.
8. **APT reprepro idempotency** — pkg+ver+arch zaten varsa skip (awk eşleştirme), re-run güvenli.
9. **Cloudsmith APT paralel yayın** — `apt.yml` publish job'una eklendi, **v0.5.6 ile doğrulandı (8 paket Created)**.
10. **Doküman denetimi** — README'ye Cloudsmith kurulum step'i + stale değer düzeltmeleri (aşağıda).

---

## Cloudsmith durumu (KRİTİK BİLGİLER)

- **Repo:** `sentirum/pg_ask` (OSS tipi, Broadcast public)
- **Broadcast sayfası:** https://broadcasts.cloudsmith.com/sentirum/pg_ask
- **Secret:** GitHub **org-level** secret adı `CLOUDSMITH_TOKEN` (All repositories).
  Workflow'da env olarak `CLOUDSMITH_API_KEY: ${{ secrets.CLOUDSMITH_TOKEN }}` veriliyor.
- **CLI:** `apt.yml`'de `pipx install cloudsmith-cli` ile kuruluyor (publish job içinde).
- **APT push komutu (çalışıyor):**
  ```
  cloudsmith push deb --no-wait-for-sync sentirum/pg_ask/<distro>/<codename> file.deb
  ```
  Codename→distro eşlemesi: `bookworm|trixie → debian`, `jammy|noble → ubuntu`.
- **Idempotency:** "already exists/conflict/duplicate" tolere ediliyor, başka hatada fail.

### Sıradaki iş için Cloudsmith komutları (henüz kullanılmadı)
- RPM:    `cloudsmith push rpm    sentirum/pg_ask/<distro>/<version> file.rpm`
- APK:    `cloudsmith push alpine sentirum/pg_ask/<distro>/<version> file.apk`
- Docker: OCI registry `docker.cloudsmith.io/sentirum/pg_ask/<image>:<tag>`
  (`cloudsmith push docker ...` veya `docker push` + login). Komut/format
  yeni oturumda resmi doc'tan TEYİT EDİLMELİ.

---

## Paketleme envanteri (mevcut)

| Format | Durum | Dosya(lar) |
|--------|-------|-----------|
| APT (.deb) | ✅ var, gh-pages + Cloudsmith | `packaging/debian/build-deb.sh`, `.github/workflows/apt.yml` |
| Docker | ✅ var, GHCR'ye push | `Dockerfile`, `.github/workflows/docker.yml` (`ghcr.io/sentirum/pg_ask:VERSION-pg18`) |
| RPM | ❌ YOK — sıfırdan | — |
| APK | ❌ YOK — sıfırdan | — |

Desteklenen APT hedefleri: Debian bookworm/trixie + Ubuntu jammy/noble, `amd64`+`arm64`.

---

## Doküman düzeltmeleri (yapıldı, referans için)

- **README:** Cloudsmith tek-satır kurulumu ÖNERİLEN yol; gh-pages `<details>` alternatif.
  `curl -sLf https://dl.cloudsmith.io/public/sentirum/pg_ask/cfg/setup/bash.deb.sh | sudo bash`
- Stale değerler (tüm dosyalarda): test sayısı **75→90**, `status()` örneği **0.5.4→0.5.6**,
  `max_iterations` default **16→24** (README config tablo + limits, ARCHITECTURE, SECURITY).
- **ROADMAP:** "(current)" v0.5.6'ya taşındı + v0.5.3–0.5.6 özeti; APT repo "shipped";
  "remaining" = RPM+APK+Docker'ı Cloudsmith'te single-source + gh-pages emekli.

---

## Plan: RPM + APK + Docker → Cloudsmith (Faz 1 = paralel yayın)

**Faz 1 (yapılacak):** mevcut kanallara DOKUNMADAN Cloudsmith'e paralel yayın.
1. **RPM** — pgrx `cargo pgrx package` çıktısından `.rpm` üret (`.spec` veya `fpm`).
   RedHat/Fedora hedefleri. Yeni workflow ya da mevcut bir job'a matris.
2. **APK** — Alpine `APKBUILD` (musl! pgrx'in musl derlemesi araştırılmalı — RİSK).
3. **Docker** — mevcut `docker.yml`'i Cloudsmith OCI registry'ye de push edecek şekilde
   genişlet (GHCR'ye dokunma, paralel ekle).

**Faz 2 (sonra, ACELE YOK):** gh-pages apt reposunu emekliye ayır, tek source Cloudsmith.
Önce birkaç sürüm paralel kalsın + gerçek `apt install` / `yum install` / `apk add` testi.

### Dikkat / riskler
- **APK/musl:** pgrx + PostgreSQL extension'ı musl/Alpine'de derlemek zahmetli olabilir.
  Önce fizibilite kontrol et; gerekirse APK'yı erteleyip RPM+Docker ile başla.
- **Reproducible build değil** → idempotency koruması her formatta gerekli (re-run güvenliği).
- Her yeni format için codename/distro→Cloudsmith path eşlemesi netleştirilmeli.

---

## Ortam notları

- **Dev makinede `cargo-pgrx` KURULU DEĞİL** (`~/.pgrx` yok). Clippy/pg_test lokalde çalışmaz.
- **Testler OrbStack Docker ile:** `docker compose -f docker-compose.test.yml up --build -d`
  sonra `./tests/run-fixture-tests.sh`. (Docker Desktop yok, OrbStack var. Şu an kapalı.)
- Lisans: **PostgreSQL License** (OSS → Cloudsmith free tier uygun).
- CI: rustfmt + clippy `-D warnings` + `cargo pgrx test pg18`. PR gate PG runtime istemez.

## İlgili context anchor
`docs-refreshed-cloudsmith-step` — yeni oturumda `context recall keyword=cloudsmith` ile bulunabilir.
