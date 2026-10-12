# WuwaLang

Wuthering Waves için Türkçe çeviri paketi. Oyunun İngilizce veritabanlarındaki metinleri
çeviri motorlarıyla (Yandex, Google, OpenAI uyumlu veya Anthropic) Türkçeye çevirir,
yerel bir çeviri belleğinde saklar ve oyunun kendi paket formatında (WuWa V12) derleyip kurar.

## Lisans ve atıf

- Bu proje **GPL-3.0-or-later** lisanslıdır; tam metin [LICENSE](LICENSE) dosyasındadır.
- Temel alınan kaynak: [GishReloaded/WuWaRu](https://github.com/GishReloaded/WuWaRu) (GPL-3.0-or-later).
  `src/game.rs` (kurulum, çıkarma, derleme, kurma) ve `src/pak.rs` (WuWa V12 paket yazıcısı) bu projeden uyarlanmıştır.
  Çeviri motoru katmanı, sözlük, çeviri belleği, arayüz ve CLI bu projeye özgüdür.
- Oyun verisini okumak için kullanılan FModelCLI v1.0.2 (GPL-3.0) `wuwalang setup` komutuyla resmi sürümden indirilir
  ve SHA-256 ile doğrulanır. İkili dosya depoya eklenmez.
  Bkz. https://github.com/Herselfta/FModelCLI
- Wuthering Waves ve ilgili tüm varlıklar Kuro Games'in mülkiyetindedir. Bu proje bir resmi ürün değildir.

## Uyarı

Kuro Games'in Fair Play kuralları oyun verisinin değiştirilmesini yasaklar. Bu aracı kullanmanın
riski (hesap kısıtlaması dahil) tamamen kullanıcıya aittir. Ana hesabınızla kullanmayın; ayrı bir hesap tercih edin.

## Hızlı başlangıç (exe)

1. `wuwalang.exe` dosyasına argüman vermeden çift tıklayın; kurulum penceresi açılır.
2. **Gözat** ile `Wuthering Waves.exe` (veya `Client-Win64-Shipping.exe`) dosyasını seçin.
3. İlk açılışta Fair Play uyarısını okuyup onaylayın.
4. Çeviri sekmesinden bir API anahtarı girip **Çevir** ile eksik metinleri çevirin (ilk kurulumda gerekli). Çeviri otomatik başlamaz.
5. **Kur** düğmesine basın: araçlar, çıkarma, topluluk paketi, derleme ve kurulum tek seferde yapılır. Kur çeviri yapmaz, yalnızca mevcut çeviri belleğini kullanır; çeviri belleği boşsa paket kurulmaz.
6. Oyunu açmadan önce kapatın; kaldırmak için **Kaldır** kullanın.

## İndirme

- Hazır derleme: GitHub Releases sayfasından `wuwalang.exe` dosyasını indirin.
- Kaynaktan derleme (Rust araç zinciri gerekir):

```powershell
cargo build --release
```

Derlenen dosya `target\release\wuwalang.exe` yoluna çıkar.

## Gereksinimler

- Windows 10/11 (oyun ve kurulum Windows içindir; Linux'ta yalnızca metin/çeviri adımları ve testler çalışır)
- Yüklü ve güncel Wuthering Waves (Steam veya resmi başlatıcı)

## Araçların hazırlanması

```powershell
wuwalang setup
```

`setup` FModelCLI'yi indirir ve doğrular, `pakkeys.txt` anahtar dosyasını hazırlar. Araçlar
`tools` altına yazılır. `extract` komutu bu dosyalar eksikse onları kendiliğinden hazırlar.
Oyun güncellemesinden sonra anahtarları yenilemek için `wuwalang setup --refresh-keys` çalıştırın.
Arayüzde de **Kur** sekmesindeki **Araçları hazırla** düğmesi aynı işi yapar.

## API anahtarı ve yapılandırma

`config.example.json` dosyasını `config.json` olarak kopyalayın (git'e eklenmez). Anahtarlar
dosyada tutulabilir ya da ortam değişkeni olarak verilebilir:

| Motor       | Ortam değişkeni                | Not                                           |
|-------------|--------------------------------|-----------------------------------------------|
| `yandex`    | `WWTR_YANDEX_API_KEY`          | `yandex.folder_id` da gerekir                 |
| `google`    | `WWTR_GOOGLE_API_KEY`          | Cloud Translation v2                          |
| `openai`    | `WWTR_OPENAI_API_KEY`          | `openai.base_url` ve `openai.model` gerekir   |
| `anthropic` | `WWTR_ANTHROPIC_API_KEY`       | `anthropic.model` gerekir                     |

Yandex ve Google makine çevirisidir: etiketler ve sözlük terimleri jetonlarla korunur.
OpenAI uyumlu ve Anthropic motorları yapay zekâ çevirisidir: model, yeni karşılaşılan özel
isimleri `new_terms` alanında önerir; bu öneriler `glossary.json` dosyasına eklenir.

## Akış

```powershell
wuwalang setup                      # FModelCLI ve pakkeys.txt hazırla (bir kez)
wuwalang extract                    # oyundan İngilizce veritabanlarını çıkar
wuwalang translate --dry-run        # API çağırmadan tahmin (paket, karakter, TM isabeti, token)
wuwalang translate --limit 50       # önce küçük bir deneme
wuwalang translate                  # eksik metinleri çevir
wuwalang build                      # çeviri belleğinden paketi derle ve doğrula
wuwalang install                    # paketi oyuna kur (oyun kapalıyken)
```

Kurulumdan sonra oyunda dil olarak **English** seçilir; Türkçe metinler bu dil yuvasından yüklenir.
`uninstall` komutu paketi kaldırıp İngilizce dili geri yükler.

### Oyun güncellemesinden sonra

```powershell
wuwalang setup --refresh-keys       # yalnızca oyun güncellenmişse
wuwalang extract
wuwalang import-pack                # topluluk paketi: kaynak metni değişmeyen satırlar alınır
wuwalang translate                  # yalnızca yeni veya değişmiş metinler çevrilir
wuwalang build
wuwalang install
```

Kaynak metni değişen satırlar paketten atlanır ve yeniden çevrilir.

Çeviri, kaynak veya paket işlemi bittiğinde değişiklik varsa çeviri belleği otomatik yedeklenir (bkz. Yedekler).
Bu yüzden güncellemeden önce `wuwalang backup` ile de elle bir yedek alabilirsiniz.

## Sözlük

```powershell
wuwalang glossary list
wuwalang glossary add "Rover" "Gezgin" --locked
wuwalang glossary lock "Jiyan"
```

Kilitli terimler otomatik önerilerle değişmez. Modelin farklı bir karşılık önerdiği durumlar
`conflicts.log` dosyasına yazılır; mevcut karar korunur. Sözlük `glossary.json` dosyasındadır.

Sözlüğe yalnızca özel isimler girer: karakter, yer, grup, eşya, beceri ve oyun mekaniği terimleri.
Modelin önerdiği girdi en fazla 4 kelime olmalı ve cümle işareti içermemelidir; aksi halde atlanır.
Eski sürümden kalan cümle benzeri girdiler için:

```powershell
wuwalang glossary clean --dry-run   # yalnızca say ve listele
wuwalang glossary clean             # geçersiz ve kilitsiz girdileri sil (önce yedek alınır)
```

Kilitli girdiler her durumda korunur.

## Topluluk paketi

Çeviri belleği `packs/tr/<tablo>.jsonl` dosyalarına dışa aktarılabilir:

```powershell
wuwalang export-pack
```

Her satır `{"id": "<dosya>:<tablo>:<kimlik>", "src": "...", "tr": "..."}` biçimindedir.

## Paketler

- `export-pack` ayrıca `packs/tr/index.json` yazar: tablo dosyalarının adı, sha1 ve satır sayısı.
- `import-pack` yerel klasörden paketi çeviri belleğine alır.
- `pull-pack [--url URL]` `index.json` üzerinden repodaki paketi çeker. Değişmeyen dosyalar (sha1, `packs/.cache`) indirilmez. Varsayılan URL `config.json` içindeki `pack_url` alanıdır.
- Arayüzdeki Paketler sekmesi aynı işlemleri yapar. Kur sırasında önce yerel paket içe aktarılır, ardından `pack_url` doluysa repodan çekilmeyi dener. Çekme başarısız olursa yalnızca uyarı yazılır. Kur çeviri başlatmaz; çevrilmemiş metinleri çevirmek için Çeviri sekmesindeki **Çevir** kullanılır. Kur bittiğinde çevrili oran günlüğe yazılır, çevrilmeyen metinler oyunda İngilizce kalır.

## Yedekler

```powershell
wuwalang backup                       # çeviri belleğinin yedeğini al
wuwalang backup list                  # yedekleri tarih, kayıt sayısı ve boyutla listele
wuwalang restore backups\tm.tr.<tarih>.db   # yedekten geri yükle (mevcut hali önce yedeklenir)
```

Yedekler `backups\tm.<kod>.<YYYYMMDD-HHMMSS>.db` olarak yazılır (saat UTC) ve SQLite'ın çevrimiçi
yedekleme komutuyla alınır. Dil başına en fazla 10 yedek tutulur. Otomatik yedek; çeviri bittiğinde
ya da durdurulduğunda, Kur, Kaldır ve Derle öncesinde, çeviri belleğinde değişiklik varsa alınır.
Kaynak veritabanı çıkarılmışsa topluluk paketi (`packs\<kod>\`) de aynı anda yazılır. Elle düzeltilen
kayıtlar yedekte ve pakette korunur.

## Arayüz (F7)

```powershell
wuwalang ui
```

Oyunun üstünde küçük bir durum etiketi açılır; **F7** ile menü açılıp kapanır. Etiket
`WuwaLang aktif (tr) · F7`, `WuwaLang pasif · F7` ya da `WuwaLang kurulu değil · F7` olarak görünür.
Menüde:

- Üstte oyunun çevrilme yüzdesi (`X / Y`). Yalnızca çıkarılmış oyun verisi üzerinden hesaplanır; oyun
  güncellenip yeni metin gelince yüzde düşer. Çeviri sırasında partiler bittikçe güncellenir.
- Oyun yolu ve **Hedef dil** seçimi. Seçim `config.json` içindeki `target` alanına kaydedilir; çeviri
  belleği, sözlük ve paket klasörleri dile göre ayrılır.
- Durum kartları (oyun, paket, TM kaydı, sözlük terimi, hedef dil, motor, yüzde) ve **Kur**, **Kaldır**,
  **Aktif et / Devre dışı bırak**, **Yeniden derle ve kur** düğmeleri. Engelli bir düğmenin üzerine gelince
  nedeni görünür.
- **Çeviri**: motor ve anahtar alanları (anahtarı göster/gizle), **Bağlantıyı dene**, **Tahmin**,
  **Çevir**, **Durdur**. Durdurulan çeviri biten kısımları kayıtlı bırakır.
- **Sözlük**: anlık arama, filtre, yerinde düzenleme, kilit ve silme (onaylı), yeni terim ve
  **Sözlüğü temizle**.
- **Düzenleyici**: çeviri belleğinde kaynak ya da çeviri araması, elle düzeltme ve **Sıfırla**
  (yeniden çevrilsin). Elle düzeltilen metin oyunda geçtiği her yerde değişir; oyuna yansıması için
  **Yeniden derle ve kur** gerekir. Etiket ya da `{0}` kümesi uyuşmazsa uyarı çıkar ve onay istenir.
- **Yedek**: şimdi yedekle, listele, geri yükle (onaylı).
- **Paketler**: dışa aktar, içe aktar, repodan çek, derle.

Bindirme oyun penceresinin sağ üst köşesinde ayrı bir pencere olarak durur. Oyunun **Borderless** ya da
**Windowed** modda olması gerekir; özel tam ekran modunda bindirme görünmez. Oyuna enjeksiyon yapılmaz,
oyun belleğine dokunulmaz; arayüz kendi sürecinde çalışır.

## Testler

```powershell
cargo test
```

Testler ağa çıkmaz; HTTP çağrıları taklit edilir.

## Proje yapısı

| Yol                    | İçerik                                                     |
|------------------------|------------------------------------------------------------|
| `src/main.rs`          | Komut satırı (clap)                                        |
| `src/game.rs`          | Araç hazırlama, çıkarma, derleme, kurma, kaldırma          |
| `src/pak.rs`           | WuWa V12 paket yazıcısı                                    |
| `src/batching.rs`      | TM isabeti, tekilleştirme, paketleme, çalıştırma           |
| `src/tm.rs`            | SQLite çeviri belleği ve topluluk paketi                   |
| `src/glossary.rs`      | Kalıcı sözlük                                              |
| `src/backup.rs`        | Çeviri belleği yedekleri ve geri yükleme                   |
| `src/placeholders.rs`  | Yer tutucu koruma ve denetimi                              |
| `src/prompt.rs`        | Yapay zekâ istemi ve yanıt ayrıştırma                      |
| `src/engines/`         | Yandex, Google, OpenAI uyumlu, Anthropic motorları         |
| `src/ui/`              | F7 bindirmesi ve menü                                      |
| `packs/tr/`            | Topluluk çeviri paketi                                     |
| `glossary.json`        | Sözlük                                                     |

## Lisans

Bu proje GPL-3.0-or-later altında dağıtılır. Ayrıntılar için [LICENSE](LICENSE) dosyasına bakın.
