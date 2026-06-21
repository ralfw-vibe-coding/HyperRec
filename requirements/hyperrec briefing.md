# HyperRec – Produkt- und Architekturbriefing

## Projektname

HyperRec

---

# Vision

HyperRec ist ein minimalistischer Audio Recorder für macOS.

Die Anwendung soll Gespräche und Computeraudio mit möglichst wenig Konfiguration zuverlässig aufzeichnen.

Der Nutzer soll HyperRec öffnen, auf „Aufnahme starten“ klicken und danach nicht mehr über Technik nachdenken müssen.

HyperRec ist kein Podcast-Studio, keine DAW und keine Meeting-Software.

HyperRec ist ein Werkzeug.

---

# Zielgruppe

Privater Eigengebrauch.

Die Anwendung wird nicht über den App Store vertrieben.

Es ist keine Notarisierung oder App-Store-Veröffentlichung vorgesehen.

Es wird kein Apple Developer Account benötigt.

---

# Technische Rahmenbedingungen

## Erlaubt

* Normale macOS-Berechtigungen
* Mikrofonzugriff
* Audio-/Systemaudio-Capture-Berechtigungen

## Nicht erlaubt

* BlackHole
* Loopback
* Soundflower
* Virtuelle Audiotreiber
* Kernel Extensions
* Spezielle Boot-Modi

Die Lösung soll ausschließlich auf nativen Betriebssystem-APIs basieren.

---

# Technologie-Stack

## Frontend

* Tauri 2
* TypeScript
* HTML
* CSS

## Backend

* Rust

## macOS Audio

* Core Audio
* Core Audio Process Taps für Systemaudio
* Core Audio oder cpal für Mikrofonaufnahme

## Dateiformat

Version 1:

* WAV

Keine MP3- oder AAC-Unterstützung in Version 1.

---

# Kernfunktion

Die Anwendung soll Audio aus zwei Quellen gleichzeitig aufzeichnen:

1. Mikrofon
2. Audioausgabe des Systems

Beide Quellen werden in einer einzigen WAV-Datei zusammengeführt.

---

# Aufnahmeszenarien

## Szenario A: Kein Headset angeschlossen

Aufzeichnen von:

* MacBook-Mikrofon
* MacBook-Lautsprecher-Ausgabe

---

## Szenario B: Headset angeschlossen

Aufzeichnen von:

* Headset-Mikrofon
* Headset-Audioausgabe

---

# Geräteerkennung

Beim Start werden verfügbare Audiogeräte angezeigt.

## Mikrofone

Beispiele:

* System Default
* MacBook Mikrofon
* AirPods Mikrofon
* USB Headset Mikrofon

## Audioausgabe

Beispiele:

* System Default
* MacBook Lautsprecher
* AirPods
* USB Headset

---

# Standardverhalten

Beim Start:

* aktuelles Standard-Mikrofon vorauswählen
* aktuelle Standard-Audioausgabe vorauswählen

Der Nutzer soll normalerweise nichts konfigurieren müssen.

---

# Benutzeroberfläche

## Hauptfenster

Titel:

HyperRec

Inhalt:

* Mikrofon-Auswahl
* Audioausgabe-Auswahl
* Anzeige der ausgewählten Geräte
* Start-Button

Optional:

* Link zu Einstellungen

---

# Aufnahmefenster

Sobald eine Aufnahme gestartet wird:

* separates kleines Fenster öffnen
* immer im Vordergrund (Always On Top)
* verschiebbar
* möglichst klein
* unauffällig

Anzeige:

```text
● REC

00:00:00

[Pause]
[Stop]

[HyperRec öffnen]
```

---

# Verhalten des Aufnahmefensters

Wenn Hauptfenster geschlossen wird:

* Aufnahme läuft weiter
* Aufnahmefenster bleibt sichtbar

Wenn Nutzer auf „HyperRec öffnen“ klickt:

* Hauptfenster in den Vordergrund holen

Wenn Aufnahme endet:

* Aufnahmefenster automatisch schließen

---

# Menüleisten-Integration

HyperRec soll ein Menüleisten-Icon besitzen.

## Wenn keine Aufnahme läuft

Menü:

* HyperRec öffnen
* Aufnahme starten
* Beenden

## Während Aufnahme

Menü:

* Status anzeigen
* Laufzeit anzeigen
* Pause
* Fortsetzen
* Stop

Die Anwendung soll vollständig über die Menüleiste bedienbar sein.

---

# Aufnahmezustände

```rust
enum RecordingState {
    Idle,
    Recording,
    Paused,
    Stopped,
}
```

---

# Ablauf

## Start

* Geräte prüfen
* Aufnahme starten
* Timer starten
* Aufnahmefenster anzeigen

## Pause

* Aufnahme pausieren

## Resume

* Aufnahme derselben Session fortsetzen

Keine neue Datei erzeugen.

## Stop

* Aufnahme beenden
* WAV-Datei finalisieren
* Save-Dialog öffnen

---

# Timer

Während Aufnahme sichtbar:

```text
● REC 00:42:15
```

Der Status muss jederzeit eindeutig erkennbar sein.

---

# Dateispeicherung

Während Aufnahme:

* temporäre WAV-Datei erzeugen

Nach Stop:

* macOS Save-Dialog öffnen
* Benutzer wählt Ziel
* Datei wird verschoben

---

# Standard-Dateiname

Format:

```text
YYYY-MM-DD_HH-mm-ss.wav
```

Beispiel:

```text
2026-06-20_18-45-03.wav
```

---

# Audioverarbeitung

Mikrofon und Systemaudio werden gemischt.

Anforderungen:

* gemeinsame WAV-Datei
* einheitliche Sample Rate
* Resampling falls notwendig
* Zuverlässigkeit wichtiger als maximale Audioqualität

---

# Berechtigungen

Beim ersten Start:

## Mikrofon

Falls nicht vorhanden:

„HyperRec benötigt Zugriff auf das Mikrofon.“

Button:

„Systemeinstellungen öffnen“

---

## Systemaudio

Falls nicht vorhanden:

„HyperRec benötigt Zugriff auf die Systemaudio-Aufnahme.“

Button:

„Systemeinstellungen öffnen“

---

# Plattformunabhängige Architektur

Die Anwendung soll von Anfang an für eine spätere Windows-Version vorbereitet werden.

Die UI darf keine direkten Betriebssystem-APIs verwenden.

---

# Audio Provider Architektur

```text
Tauri UI
    ↓
Recording Controller
    ↓
AudioProvider Interface
    ├── MacAudioProvider
    └── WindowsAudioProvider
```

---

# AudioProvider Interface

```rust
trait AudioProvider {
    fn list_input_devices(&self) -> Result<Vec<AudioDevice>>;
    fn list_output_devices(&self) -> Result<Vec<AudioDevice>>;

    fn default_input_device(&self) -> Result<AudioDevice>;
    fn default_output_device(&self) -> Result<AudioDevice>;

    fn check_permissions(&self) -> Result<PermissionStatus>;
    fn request_permissions(&self) -> Result<PermissionStatus>;

    fn start_recording(&mut self, config: RecordingConfig) -> Result<()>;
    fn pause_recording(&mut self) -> Result<()>;
    fn resume_recording(&mut self) -> Result<()>;
    fn stop_recording(&mut self) -> Result<RecordingResult>;
}
```

---

# Gemeinsame Komponenten

Plattformunabhängig:

* UI
* Timer
* State Machine
* Save Dialog
* WAV Writer
* Menüleistenfunktion
* Aufnahmefenster
* Fehlerbehandlung
* Einstellungen

---

# macOS Provider

Implementierung über:

* Core Audio
* Core Audio Process Taps
* Core Audio oder cpal

Keine Drittanbieter-Audiotreiber.

---

# Windows Provider (später)

Implementierung über:

* WASAPI
* WASAPI Loopback Capture

Die Architektur muss dies ermöglichen, ohne UI oder Recording-Logik anzupassen.

---

# Einschränkungen Version 1

Es darf immer nur eine Aufnahme gleichzeitig geben.

Keine parallelen Sessions.

Keine Projektverwaltung.

Keine Tabs.

Keine Mehrspuraufnahmen.

Keine Cloud-Funktionen.

Keine Transkription.

Keine KI-Funktionen.

Keine Benutzerkonten.

Keine Synchronisation.

Keine Audio-Bearbeitung.

Keine MP3-Unterstützung.

---

# Definition of Done

Version 1 ist fertig, wenn:

1. Geräte erkannt werden
2. Standardgeräte automatisch ausgewählt werden
3. Mikrofon aufgenommen wird
4. Systemaudio aufgenommen wird
5. Beide Quellen gemischt werden
6. Aufnahme gestartet werden kann
7. Pause funktioniert
8. Resume funktioniert
9. Stop funktioniert
10. Timer sichtbar ist
11. Always-on-top-Aufnahmefenster funktioniert
12. Menüleistensteuerung funktioniert
13. WAV-Datei gespeichert werden kann
14. Keine virtuellen Audiotreiber benötigt werden
15. Die Architektur für einen späteren Windows-AudioProvider vorbereitet ist

---

# Klärungen / Entscheidungen (Ergänzung)

## Mindest-macOS-Version

macOS 14.4 (Sonoma) oder neuer.

Begründung: Core Audio Process Taps stehen erst ab macOS 14.4 zur Verfügung. Da rein private Nutzung auf eigenem, aktuellem Gerät (macOS Tahoe 26.5) vorgesehen ist, ist diese Grenze unkritisch.

---

## Pause-Verhalten

Beim Pausieren wird kein Audio (auch keine Stille) für die Pausendauer geschrieben.

Die resultierende WAV-Datei enthält nur tatsächlich aufgezeichnetes Audio, keine Lücken oder Stille-Abschnitte für die Pausenzeit.

---

## Save-Dialog / Temporäre Datei

* Nach Stop kann die Aufnahme beliebig oft erneut über den Save-Dialog gespeichert werden.
* Die interne temporäre Datei bleibt bestehen, bis die Anwendung vollständig beendet wird.
* Beim Beenden der Anwendung wird die temporäre Datei aufgeräumt (kein Müll im Temp-Verzeichnis).

---

## Geräte-Hotswap während Aufnahme

Fällt das aktive Mikrofon oder die aktive Audioausgabe während der Aufnahme weg (z. B. Headset-Akku leer), wechselt die Aufnahme automatisch auf das nächste verfügbare Gerät desselben Typs (z. B. zurück auf MacBook-Mikrofon) und läuft ohne Abbruch weiter.

Beispiel: Headset fällt im Meeting aus → Aufnahme läuft nahtlos mit MacBook-Mikrofon weiter.

---

## Pegel der Mischung

Mikrofon und Systemaudio werden beim Mischen ungefähr pegelangeglichen (vergleichbare Lautheit), ohne hochwertiges Audio-Mastering. Einfache, robuste Lösung statt aufwändiger Lautheitsmessung.

---

## Temporäres Verzeichnis

Es gibt ein anwendungsspezifisches Temp-Verzeichnis (z. B. `~/Library/Application Support/HyperRec/tmp`) für die laufende Aufnahme.

---

## Verhalten bei Absturz

Bei einem Absturz der Anwendung während einer Aufnahme geht die Aufnahme verloren. Für Version 1 ist keine Recovery-Mechanik (z. B. fortlaufend valider WAV-Header) vorgesehen. Einfachste Lösung hat Vorrang.

---

## Always-on-Top – pragmatische Lösung

Das Aufnahmefenster soll im Normalfall immer im Vordergrund bleiben, muss sich aber nicht gegen Vollbild-Modi anderer Anwendungen durchsetzen (z. B. PowerPoint-Präsentationsmodus). Kein Anspruch auf Sichtbarkeit über Fullscreen-Spaces hinweg.

---

## Einstellungen-Fenster

Es gibt ein App-Settings-Fenster. Mögliche Inhalte u. a.:

* Pfad des temporären Verzeichnisses

Weitere Einstellungen können bei Bedarf ergänzt werden.

---

## Sample Rate / Bit Depth

Ziel: eine übliche, robuste Standard-Sample-Rate, keine High-Fidelity-Anforderung, da Quellen (z. B. Online-Meeting-Audio) ohnehin oft komprimiert/qualitätsreduziert sind.

Empfehlung: 48 kHz, 16-bit PCM.

---

## Keine Menüleisten-Integration (Revision)

Entgegen der ursprünglichen Anforderung ("Menüleisten-Integration") wird **kein** Menüleisten-Icon (oben rechts, neben Uhr/WLAN) benötigt.

Das normale Dock-Icon (unten, wie bei jeder anderen App) reicht aus.

Punkt 12 der Definition of Done ("Menüleistensteuerung funktioniert") entfällt damit.

---

## Radikal vereinfachtes UI (Revision, 2026-06-21)

Die ursprüngliche Zwei-Fenster-Architektur (Hauptfenster + separates Always-on-Top-Aufnahmefenster) entfällt vollständig. Es gibt nur noch **ein** Fenster.

### Fenster = App

Das Fenster ist die gesamte Anwendung. Schließen des Fensters beendet die App vollständig (kein Hide-and-reopen, kein Dock-Verhalten). Eine laufende Aufnahme endet dabei mit, temporäre Dateien werden gelöscht.

Always-on-Top ist nicht mehr erforderlich.

### Vier Zustände in einem Fenster

```text
ready      → Geräteauswahl sichtbar, Record-Button (●)
recording  → Geräteauswahl ausgeblendet, Stop (■) + Pause (‖)
paused     → Geräteauswahl wieder sichtbar, Stop (■) + Resume (▶)
recorded   → Geräteauswahl ausgeblendet, Download (⬇) + Verwerfen (🗑)
```

Timer ist in allen Zuständen sichtbar (zählt nur tatsächliche Aufnahmezeit, keine Pausenzeit).

### Kein automatischer Save-Dialog mehr nach Stop

Nach Stop landet die App im Zustand `recorded`. Der Nutzer entscheidet explizit:

* **Download** (⬇): öffnet den nativen Speichern-Dialog, kopiert die Datei an den gewählten Ort. Kann mehrfach gedrückt werden (Datei bleibt nutzbar).
* **Verwerfen** (🗑): löscht die temporäre Datei sofort und unwiderruflich, zurück zu `ready`.

### Optik

Schlicht, Schwarz auf Weiß, 2px-Linien, keine Schatten/Farben, kompaktes Fenster (kein 800×600-Vollfenster mehr).
