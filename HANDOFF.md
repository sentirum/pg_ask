# HANDOFF — pg_ask çalışma durumu

> Bu dosya bir oturum devri (session handoff) içindir. Yeni oturum bunu
> okuyup kaldığı yerden sorunsuz devam etmeli. Tarih: 2026-06-06.

## TL;DR — sıradaki iş

**Faz 1 BİTTİ (commit edildi, CI doğrulaması bekliyor).** RPM + APK + Docker
artık Cloudsmith'e paralel yayınlanıyor. Build scriptleri + workflow'lar lokal
test edildi (APK pgrx-musl build + abuild paketleme lokalde çalıştı; Docker &
RPM gerçek tag push'unda CI'da görülecek).

**SIRADAKİ:** Gerçek bir tag (örn. v0.5.7) push edip 3 yeni workflow'u CI'da
doğrula → Cloudsmith'te rpm/apk/docker paketlerinin oluştuğunu teyit et.
Sonra Faz 2 (gh-pages apt reposunu emekliye ayırma).

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

## Faz 1 tamamlananlar (2026-06-06, ikinci oturum)

**Docker → Cloudsmith** (`docker.yml`, GHCR'ye DOKUNULMADI):
- merge job sonuna paralel push: `docker login docker.cloudsmith.io`
  (user=`sentirum/pg_ask`, pass=`CLOUDSMITH_TOKEN`) → `imagetools create` ile
  aynı per-arch digest'lerden cross-registry manifest push (rebuild yok).
- Image: `docker.cloudsmith.io/sentirum/pg_ask/pg_ask:<ver>-pg18` + `latest-pg18`.

**RPM → Cloudsmith** (sıfırdan, `packaging/rpm/build-rpm.sh` + `.github/workflows/rpm.yml`):
- `build-rpm.sh` = `build-deb.sh` simetriği: `cargo pgrx package` staging tree →
  `rpmbuild -bb` (hand-populated buildroot, NO `%prep`/`%setup`).
- Paket adı `pg_ask_18` (PGDG convention). license/changelog buildroot'a manuel
  kopyalanıyor (çünkü `%doc`/`%license` makrosu `%_builddir` arar, o yok).
- `debug_package`/strip kapalı (Rust cdylib). arch map: amd64→x86_64, arm64→aarch64.
- Matrix: rockylinux:9 (el9), rockylinux:8 (el8), fedora:40 (fc40) × amd64/arm64
  = 6 job; native arm64 runner; PGDG repo + `postgresql18-devel`;
  `dnf module disable postgresql` (el8/9).
- Push: `cloudsmith push rpm sentirum/pg_ask/{el/9,el/8,fedora/40}` (filename'den
  disttag sed ile çıkarılıyor); idempotency 422 skip.
- ⚠️ Lokal test EDİLMEDİ (RHEL container gerekli). Risk: el8 eski clang/glibc;
  PG18 PGDG availability ilk run'da görülür.

**APK → Cloudsmith** (sıfırdan, `packaging/alpine/build-apk.sh` + `.github/workflows/apk.yml`):
- ⚠️ **PG18-dev sadece Alpine `edge`'de** (stable 3.20-3.22'de max PG17). Bu yüzden
  base image `alpine:edge`, Cloudsmith distro `alpine/edge`. PG18 stable'a düşünce
  matrix'e eklenir.
- pgrx + crate **musl'da SORUNSUZ derlendi** (lokal Docker testi). **rustfmt Alpine'de
  AYRI paket** (`rust` pkg içermiyor), pgrx-bindgen onu çağırıyor → eklendi.
- `build-apk.sh` = abuild ile paketler; `package()` içinde `mkdir -p $pkgdir`
  (source='' boşken pkgdir oluşmuyordu) + `SKIP_PGRX_PACKAGE=1` flag (staged reuse).
  Throwaway abuild key CI'da `abuild-keygen -a -n` ile üretiliyor.
- Paket adı `pg_ask18`, depend `postgresql18`. Lokal test: `pg_ask18-0.5.6-r0.apk`
  doğru payload ile üretildi ✓.
- Push: `cloudsmith push alpine sentirum/pg_ask/alpine/edge`.

**Diğer:** `.gitignore`'a `/dist` eklendi; build scriptleri `chmod +x`; tüm YAML valid.

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

### Cloudsmith komutları (Faz 1'de doğrulanmış formatlar)
- RPM:    `cloudsmith push rpm    sentirum/pg_ask/{el/9,el/8,fedora/40} file.rpm`
- APK:    `cloudsmith push alpine sentirum/pg_ask/alpine/edge file.apk`
- Docker: `docker push docker.cloudsmith.io/sentirum/pg_ask/pg_ask:<tag>`
  (login: registry=`docker.cloudsmith.io`, user=`sentirum/pg_ask`, pass=API key).
  Cross-registry kopya `docker buildx imagetools create` ile yapılıyor.

---

## Paketleme envanteri (mevcut)

| Format | Durum | Dosya(lar) |
|--------|-------|-----------|
| APT (.deb) | ✅ var, gh-pages + Cloudsmith | `packaging/debian/build-deb.sh`, `.github/workflows/apt.yml` |
| Docker | ✅ var, GHCR'ye push | `Dockerfile`, `.github/workflows/docker.yml` (`ghcr.io/sentirum/pg_ask:VERSION-pg18`) |
| RPM | ✅ Cloudsmith (CI doğrulaması bekliyor) | `packaging/rpm/build-rpm.sh`, `.github/workflows/rpm.yml` |
| APK | ✅ Cloudsmith (CI doğrulaması bekliyor) | `packaging/alpine/build-apk.sh`, `.github/workflows/apk.yml` |
| Docker (CS) | ✅ Cloudsmith paralel (CI doğrulaması bekliyor) | `docker.yml` (`docker.cloudsmith.io/sentirum/pg_ask/pg_ask`) |

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

## Plan: Faz 1 BİTTİ → Faz 2 (sonra)

**Faz 1 (✅ tamam):** RPM + APK + Docker Cloudsmith'e paralel yayın — yukarıda detay.

**SIRADAKİ ADIM — CI doğrulaması:** Gerçek tag push (v0.5.7) ile 3 workflow'u
çalıştır → Cloudsmith'te paketleri teyit et. Beklenen ilk-run sürprizleri:
- RPM el8: eski clang/glibc ile pgrx 0.18.1 derlemesi patlayabilir (gerekirse el8 düşür).
- RPM: `postgresql18-server`/`-devel` PGDG yum'da el8/el9/fc40 için mevcut olmalı.
- APK: `alpine:edge` hareketli hedef; PG18 stable'a düşünce `ALPINE_BRANCH` güncelle.

**Faz 2 (sonra, ACELE YOK):** gh-pages apt reposunu emekliye ayır, tek source Cloudsmith.
Önce birkaç sürüm paralel kalsın + gerçek `apt install` / `yum install` / `apk add` testi.

### Dikkat / riskler
- **Reproducible build değil** → idempotency koruması her formatta var (re-run güvenli).
- APK base `alpine:edge` (PG18 stable'da yok) — sürüm asimetrisi geçici.

---

## Ortam notları

- **Dev makinede `cargo-pgrx` KURULU DEĞİL** (`~/.pgrx` yok). Clippy/pg_test lokalde çalışmaz.
- **Testler OrbStack Docker ile:** `docker compose -f docker-compose.test.yml up --build -d`
  sonra `./tests/run-fixture-tests.sh`. (Docker Desktop yok, OrbStack var. Şu an kapalı.)
- Lisans: **PostgreSQL License** (OSS → Cloudsmith free tier uygun).
- CI: rustfmt + clippy `-D warnings` + `cargo pgrx test pg18`. PR gate PG runtime istemez.

## İlgili context anchor
`docs-refreshed-cloudsmith-step` — yeni oturumda `context recall keyword=cloudsmith` ile bulunabilir.
