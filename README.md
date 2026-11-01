# 보안뉴스 침해사고 브리핑

보안뉴스의 공식 RSS에서 기사를 수집하고, 최근 24시간에 보도된 침해사고를 최대 5개 선정하는 Rust CLI입니다. SQLite에 수집·전송 이력을 저장하며, JSON/Markdown 보고서와 선택적 디스코드 웹훅 전송을 제공합니다.

최종 전달 대상은 Discord Webhook입니다. SQLite는 같은 기사를 다시 보내지 않고 프로그램 재시작 뒤에도 예약·전송 상태를 복구하기 위한 로컬 저장소입니다. 별도 DB 서버나 접속 계정은 필요하지 않으며 제목·RSS 요약·링크·발행 시각·전송 상태를 `data/news.sqlite3` 한 파일에 보관합니다.

## NAS Docker 운영 (권장 배포 방식)

Rust 실행 파일 하나를 Linux 컨테이너 하나로 실행합니다. 별도 DB 컨테이너, 웹 서버, 포트 개방, 컨테이너 내부 cron은 필요하지 않습니다. 실행 파일 자체가 60분마다 RSS를 수집하고 매일 18시 KST에 Discord로 전달합니다.

| 항목 | 제공 설정 |
| --- | --- |
| 이미지 | Rust 1.99.1 / Debian bookworm 다단계 빌드, 실행 이미지에는 컴파일러 미포함 |
| 실행 계정 | root가 아닌 UID/GID `10001:10001` |
| 저장소 | `news-data` Docker 볼륨 → `/app/data`; SQLite·전송 이력·보고서 유지 |
| 웹훅 | `.secrets/discord_webhook_url` → 읽기 전용 Compose secret |
| 재시작 | `unless-stopped`; NAS의 Docker 서비스도 부팅 시 시작되어야 함 |
| 종료 | `SIGTERM` 처리, 진행 중인 HTTP 재시도를 위해 최대 6분 유예 |
| 로그 | stdout/stderr, Docker 로그 파일당 10 MB / 최대 3개 |
| 네트워크 | 외부 DNS·HTTPS 연결만 필요, 수신 포트 없음 |

`Dockerfile`은 CPU 아키텍처를 고정하지 않으므로 NAS에서 직접 빌드하면 해당 호스트 플랫폼을 사용합니다. x86_64와 arm64용 기반 이미지가 제공되지만, 실제 NAS 모델에서의 빌드·실행은 별도로 확인해야 합니다. 다른 PC에서 빌드한 이미지를 가져올 때는 NAS와 같은 플랫폼으로 빌드하세요. 메모리가 부족한 NAS는 Compose의 `BUILD_JOBS`를 `1`로 낮추거나 다른 빌드 머신을 사용하세요.

### 설치와 첫 실행

NAS에 Docker Engine과 Compose v2(또는 이를 제공하는 NAS 컨테이너 관리 앱)가 필요합니다. 프로젝트 소스를 NAS의 작업 폴더로 복사합니다. 기존 PC의 `target/`, `data/`, 비밀 파일은 이미지 빌드에 포함되지 않습니다. 아래 명령은 **NAS의 Linux 셸에서 프로젝트 폴더를 기준으로** 실행합니다.

1. `.secrets` 디렉터리를 만들고, 편집기로 `.secrets/discord_webhook_url` 파일을 생성합니다. UTF-8(BOM 없음) 한 줄에 실제 웹훅 URL만 넣습니다. 따옴표나 `DISCORD_WEBHOOK_URL=` 접두사는 넣지 마세요. URL을 셸 명령에 직접 입력하면 명령 이력에 남을 수 있습니다.
2. 호스트의 비밀 파일 권한을 설정합니다. 기본 구성은 rootful Docker와 컨테이너 UID 10001을 전제로 합니다. Compose의 파일 기반 secret은 호스트 파일을 마운트하므로 실제 파일 권한이 중요합니다.

```sh
chmod 700 .secrets
sudo chown 10001:10001 .secrets/discord_webhook_url
sudo chmod 400 .secrets/discord_webhook_url
docker compose config --quiet
docker compose build
# 최초 확인: 수집·미리보기만 실행하며 Discord에는 보내지 않음
docker compose run --rm news run --once
# 실제 운영 시작: --send가 설정되어 있음
docker compose up -d
docker compose logs --tail 100 -f news
```

파일은 NAS 관리자나 Docker 관리자에게는 보일 수 있습니다. `.secrets/`는 Git과 빌드 컨텍스트에서 제외되며 이미지에 복사되지 않습니다. rootless Docker, 사용자 네임스페이스 재매핑 또는 NAS ACL을 쓰면 해당 환경에 맞게 파일의 읽기 권한을 조정해야 합니다. `Permission denied`를 해결하려고 컨테이너 전체를 root로 실행하거나 비밀 파일을 전체 공개하지 마세요.

18시 KST 이후 `up -d`로 시작하면 그날 미완료 브리핑이 즉시 발송될 수 있습니다. 위의 미리보기와 실제 발송의 일일 완료 기록은 별개입니다. `TZ` 환경 변수와 관계없이 예약 계산은 코드에서 KST로 고정합니다.

### 데이터 유지·점검·업데이트

같은 Compose 프로젝트 이름을 유지하세요. 프로젝트 이름을 바꾸면 다른 이름의 새 볼륨이 생성되어 기존 전송 이력을 찾지 못할 수 있습니다. `docker compose down`은 기본적으로 이름 있는 볼륨을 지우지 않지만, **`down -v`나 볼륨 정리는 DB·전송 이력을 삭제할 수 있으므로 사용하지 마세요.** 기존 Windows `data/`는 자동 이관되지 않습니다.

NAS의 특정 폴더에서 데이터를 직접 보고 싶다면 `news-data:/app/data`를 `/실제/NAS/전용폴더:/app/data` 형태의 bind mount로 바꿀 수 있습니다. 그 전용 폴더에 UID/GID 10001의 쓰기 권한을 미리 부여하세요. NAS 자체 로컬 파일시스템을 사용하고, SQLite 파일을 다른 서버의 SMB/NFS 공유에 두지 마세요. 설정을 바꾸면 기존 볼륨 데이터도 별도로 이관해야 합니다.

같은 데이터 볼륨을 쓰는 프로세스는 한 개만 실행합니다. 점검 명령도 데몬을 먼저 중지해야 합니다.

```sh
docker compose stop news
docker compose run --rm news delivery-status
# Discord 채널에서 실제 도착 여부를 확인한 후 둘 중 하나만 실행
docker compose run --rm news resolve-delivery --delivered
# 또는: docker compose run --rm news resolve-delivery --not-delivered
docker compose up -d
```

코드 업데이트 후에는 `docker compose up -d --build`로 컨테이너를 교체합니다. 데이터 볼륨은 유지됩니다. DB 백업은 컨테이너를 중지한 뒤 `/app/data` 전체를 NAS 백업 기능으로 복사하는 방식이 단순합니다. 보고서는 자동 삭제하지 않으므로 데이터 볼륨의 사용량은 별도로 관리하세요.

자동 재시작은 **프로세스가 종료된 경우**를 위한 것입니다. 살아 있는 프로세스의 수집 실패·전송 보류까지 해결하지 않으며 현재 별도 서비스 상태 확인(healthcheck)은 제공하지 않습니다. 앞서 검토한 Discord 429 자동 복구, 정정 알림, 장기 중단분 자동 보완은 아직 구현되지 않았습니다. Docker 적용이 그 제한을 해소하지는 않습니다.

설정 근거: [Docker Compose secrets](https://docs.docker.com/compose/how-tos/use-secrets/), [볼륨 수명](https://docs.docker.com/engine/storage/volumes/), [Compose 서비스 설정](https://docs.docker.com/reference/compose-file/services/), [Rust 종료 신호 처리](https://docs.rs/ctrlc/latest/ctrlc/).

## 로컬 개발용 빠른 실행

Windows에 Rust stable과 Visual Studio C++ Build Tools가 필요합니다. [공식 Rust 설치 안내](https://rust-lang.org/tools/install)를 참고하세요. 이 작업 환경에는 Rust, Cargo, rustfmt, Clippy가 설치되었습니다. 새 터미널에서 `cargo --version`으로 확인할 수 있습니다.

```powershell
cd .\BoanNew_Crawling_and_Discord_hook
cargo run --locked -- run --once
```

이 명령은 RSS를 한 번 수집하고 로컬 보고서를 생성한 뒤 종료합니다. 설치 직후 현재 터미널에서 Cargo를 찾지 못하면 아래처럼 직접 실행할 수 있습니다.

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" run --locked -- run --once
```

## 명령

| 명령 | 동작 |
| --- | --- |
| `cargo run --locked -- collect` | RSS 1회 수집 및 저장 |
| `cargo run --locked -- digest` | 저장된 최근 24시간 기사에서 최대 5개 선정 |
| `cargo run --locked -- run --once` | 수집과 브리핑을 1회 실행 |
| `cargo run --locked -- run` | 60분마다 수집, 매일 18시 KST에 로컬 브리핑 생성 |
| `cargo run --locked -- digest --hours 48 --limit 3` | 최근 48시간 중 최대 3개 선정 |
| `cargo run --locked -- --data-dir another-data run --once` | 별도 데이터 경로 사용 |
| `cargo run --locked -- delivery-status` | 결과 확인이 필요한 전송 표시 |

편의 스크립트 `run.ps1 -Mode daemon`, `-Mode collect`, `-Mode digest`, `-Mode test`도 제공합니다. 이 환경의 PowerShell 실행 정책은 스크립트를 차단하므로 Cargo 명령을 직접 실행하면 됩니다. 시스템 실행 정책은 변경하지 않았습니다. 데몬은 현재 터미널에서 동작하며 Ctrl+C로 종료합니다. 자동 시작 서비스나 Windows 예약 작업은 등록하지 않습니다. 절전·종료 중에는 수집하지 못합니다.

`run`의 `--interval-minutes` 기본값은 60(범위 5~1440), `--digest-hour` 기본값은 18(0~23)입니다. 모든 일일 예약은 한국 시간 기준이며, 예약 시각이 지난 뒤 시작하면 수집 성공 후 그날 브리핑을 생성합니다. 날짜별 실행 이력은 재시작 뒤에도 유지됩니다. `run --once`와 `digest`는 수동 실행으로 일일 예약 이력과 별개입니다.

## 수집과 선정 방식

```text
robots.txt 확인 → 공식 RSS 수집 → URL·XML·발행 시각 검증
  → SQLite 저장·기사 ID 중복 제거
  → 최근 24시간 필터 → 침해사고 분류 → 중요도 점수·후속 기사 중복 제거
  → 최대 5개 JSON/Markdown → --send 지정 시 Discord
```

- 출처: [보안뉴스 전체기사 RSS](https://www.boannews.com/rss/allArticle.xml). 기사 전문을 추가 요청하지 않고 RSS의 제목·요약·발행 시각·링크를 사용합니다.
- 보안뉴스가 사용하는 `YYYY-MM-DD HH:mm:ss`는 KST로 해석하고, 내부 저장은 UTC로 통일합니다. RFC 2822/3339 날짜도 지원합니다. 잘못된 날짜를 현재 시각으로 대체하지 않습니다.
- 보도된 개인정보 유출, 랜섬웨어 피해, 자산 탈취, 실제 악용·공격 정황을 우선합니다. 단순 취약점 소개, 제품 홍보, 행사·교육, 칼럼 등은 제외하도록 규칙을 적용합니다.
- `사고 발생 보도`, `실제 공격 활동 보도`, `의혹·조사 단계 보도`로 구분합니다. 점수·판정 근거·기사 내 발췌가 JSON에 남습니다. 기사에 나온 주장을 다른 출처로 사실 확인한 결과는 아닙니다.
- 중요도는 사건 표현, 영향·피해 규모, 국내 관련성, 최신성 등의 휴리스틱 점수입니다. 제목 유사도와 피해 대상 단서를 이용해 같은 사건의 후속 보도를 묶습니다. 완전한 의미 분석은 아니므로 오분류·중복 누락이 가능합니다.
- 조건에 맞는 기사가 5개보다 적으면 적은 수 그대로 출력합니다. 최근 기사 부족을 이유로 기간 밖 기사나 홍보 기사를 채워 넣지 않습니다.
- 원문은 해당 링크에서 확인하세요. RSS 요약은 중간에 잘릴 수 있으며 제목·요약만으로 피해 범위나 공격 주체를 확정할 수 없습니다.

## 디스코드 전송

기본 CLI 실행은 로컬 보고서만 생성합니다. 웹훅은 환경 변수 `DISCORD_WEBHOOK_URL` 또는 파일 경로 변수 `DISCORD_WEBHOOK_URL_FILE` 중 하나로 전달하고, `--send` 또는 스크립트의 `-Send`로 전송을 명시합니다. 둘을 동시에 설정하면 오류로 종료합니다. Rust 실행 파일은 `.env` 파일을 자동 로드하지 않습니다. 제공된 Compose는 secret 파일과 `--send`를 명시하므로 실제 전송 모드입니다.

```powershell
$env:DISCORD_WEBHOOK_URL = 'https://discord.com/api/webhooks/실제_ID/실제_TOKEN'
cargo run --locked -- digest --send
# 또는 매시간 수집 + 매일 18시 전달
cargo run --locked -- run --send
```

웹훅 URL을 소스 코드·공유 설정 파일·Git에 기록하지 마세요. 비밀 파일 방식은 권한을 제한한 별도 파일만 사용합니다. 전송 모듈은 Discord의 HTTPS 웹훅만 허용하며 다른 호스트로 리다이렉트하지 않습니다. 전체 멘션은 비활성화합니다.

### Discord 카드 규격

하루 브리핑을 **Webhook 메시지 1개 + 기사별 Embed 카드 최대 5개**로 보냅니다. 메시지 일부만 전송된 상황이 생기지 않도록 기존의 단일 요청·전송 보류 처리를 유지합니다. 본문은 짧은 브리핑 안내이고, 기사별 카드는 아래 구조입니다.

```text
보안뉴스
기사 제목 (원문 링크)

핵심 변화 · 원문(RSS)
RSS에 실린 제목·요약 발췌

왜 중요한가 · 해석
해석: 사고 발생 / 공격 활동 / 의혹 단계에 따른 규칙 기반 설명

적용 조건·한계 · 수집 범위
RSS 제목·요약만 확인했으며 전문·개별 환경 영향은 검증하지 않았다는 설명

확인할 사항 · 제안
• 제안: 대상·자산 관련성 또는 공식 확인 여부 점검
• 제안: 공식 권고의 적용 범위·탐지 근거 확인
• 제안: 내부 기록과 대조

원문 제목
태그 · 규칙 기반
RSS 제목·요약 기준 · 규칙 기반 해석/제안 포함 · 기사 전문 미확인
기사 발행 시각
```

일반 `content`는 2,000자, 모든 Embed의 텍스트 합계는 6,000자 제한입니다. 구현은 이모지도 안전하도록 UTF-16 단위로 보수적으로 계산하며, 카드당 최대 2,000단위 / 5개일 때 각각 최대 1,200단위로 배분합니다. 필드당 1,024자 등 개별 제한도 지킵니다. 제목·요약 등 긴 텍스트는 `…`로 줄이되 모든 섹션과 제안 3개, 카드 제목의 원문 URL을 유지합니다. 발행 시각은 Discord가 사용자의 시간대에 맞게 표시하며, 일일 예약은 별도로 KST 기준입니다. [Discord 공식 제한](https://docs.discord.com/developers/resources/message#embed-limits)

‘적용 조건·한계’는 기사에서 가져온 주장으로 위장하지 않고 **수집 범위**로 표시합니다. 현재는 자동 번역·LLM 생성·기사 전문 수집을 수행하지 않으므로 ‘자동 번역’이나 ‘앞 18,000자 분석’이라고 표시하지 않습니다. 해석·제안은 분류별 정해진 문구이며, 예시에 나온 수준의 기사별 자유로운 분석을 생성하는 기능은 아닙니다. 태그는 원문 키워드 규칙으로 붙이며 실제 침해 사실의 추가 증거가 아닙니다.

로컬 JSON 보고서의 `schema_version`은 `2`이며 기존 `articles`와 함께 `briefings`, 실제 전송 형식의 `discord_payload`를 저장합니다. 이 미리보기에는 웹훅 URL/토큰이 들어가지 않습니다. 실제 Discord로 보내기 전에 `digest`만 실행하여 형식을 확인할 수 있습니다. 카드가 숨겨지지 않도록 `SUPPRESS_EMBEDS` 플래그는 사용하지 않습니다.

성공적으로 전송한 기사 ID는 다음 전송에서 제외합니다. 로컬 미리보기에는 이미 전송한 기사도 표시할 수 있습니다. 같은 사건의 다른 기사 ID까지 전송 이력을 영구적으로 합치지는 않으므로, 다음 날 후속 기사가 새로 선정될 수 있습니다.

전송 직전에 보류 기록을 저장합니다. 타임아웃 또는 전송 후 DB 오류가 발생하면, 실제 전달 여부가 불분명하므로 자동 재전송을 보류합니다. 데몬을 종료하고 디스코드 채널을 확인한 뒤 아래 중 해당하는 명령을 실행하세요.

```powershell
cargo run --locked -- delivery-status
# 실제 메시지가 도착한 경우
cargo run --locked -- resolve-delivery --delivered
# 실제 메시지가 도착하지 않은 것을 확인한 경우
cargo run --locked -- resolve-delivery --not-delivered
```

해제 후 `digest --send` 또는 데몬을 다시 실행합니다. 예약 전송의 보류를 전달 완료로 확정하면 기사 전송 이력과 당일 완료 날짜도 한 트랜잭션으로 저장합니다. 데몬은 일일 브리핑 직전에 새로 수집하며, 수집 실패 시 완료 처리하지 않고 재시도 대기 시간이 지난 뒤의 수집 성공을 기다립니다. 종료 요청 후에는 수집이 반환된 시점과 발송 직전에 종료 상태를 확인합니다. 진행 중인 HTTP 요청·대기 자체를 즉시 중단하는 기능은 없습니다.

## 저장 파일

```text
data/
  news.sqlite3       기사·전송 이력·일일 실행 기록
  collector.lock    동일 데이터 경로의 동시 실행 방지
  digests/
    incidents-*.json 전체 선정 결과와 근거
    incidents-*.md   읽기 쉬운 짧은 브리핑
```

같은 데이터 경로에는 한 프로세스만 접근하도록 잠급니다. 데몬 실행 중 수동 명령을 실행하려면 먼저 종료하거나 별도 `--data-dir`를 사용하세요. `data/`, 빌드 산출물, 환경 파일은 `.gitignore`에 포함됩니다. 보고서와 DB는 자동 삭제하지 않습니다.

## 검증

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

기본 테스트는 합성 기사와 로컬 실패 프록시로 XML·날짜 파싱, 외부 URL 거부, 분류 오탐·미탐, 중복 제거, 저장 트랜잭션, 일일 예약, 메시지 길이·멘션 제어를 검증합니다. 실제 Discord 전송은 테스트하지 않습니다.

Docker 배포 설정과 기사별 Embed 규격을 추가한 뒤 Windows에서 **75개 통과, 라이브 RSS 테스트 1개 기본 제외**를 확인했습니다. fmt와 Clippy도 통과했습니다. 비밀 파일 설정 6개 사례와 카드 1~5개의 전체/개별 텍스트 제한을 포함합니다. Linux용 SIGTERM 종료 테스트와 CI의 Docker 이미지 빌드·비루트 실행 검사는 추가했지만 이 환경에서는 실행하지 못했습니다. 현재 PC 및 WSL에 Docker CLI/Engine이 없어 실제 이미지 빌드, NAS 실행, Discord 카드의 실제 렌더링은 미검증입니다.

이후 수동 웹훅 연결 테스트에서 테스트 카드 1건의 생성과 Discord HTTP 200 응답을 확인했습니다. 이는 개발 PC의 별도 연결 테스트이며 Rust의 실제 뉴스 발송 전체 경로, NAS 실행 또는 정기 발송의 검증은 아닙니다. 실제 웹훅 주소는 저장소에 포함하지 않습니다.

2026-11-01 추가 5차 검증에서 자동 테스트 66개와 별도 실제 RSS 데몬 테스트 1개가 통과했습니다. 발견한 오류, 수정 내용과 검증 한계는 [VERIFICATION.md](VERIFICATION.md)에 기록했습니다. 실제 RSS를 요청하는 데몬 테스트는 아래처럼 명시적으로 실행합니다.

```powershell
cargo test --locked --offline --test runtime_verification -- --ignored --nocapture
```

여기서 `--offline`은 Cargo 의존성 다운로드만 제한합니다. 위 테스트의 실행 파일은 실제 보안뉴스 RSS에 접속하며, Discord에는 전송하지 않습니다.

아래는 추가 5차 검증 이전의 초기 구축 시점 결과입니다(Windows/MSVC·Rust 1.99.1).

- `cargo fmt --all -- --check`, `cargo clippy --locked --offline --all-targets -- -D warnings` 통과.
- `cargo test --locked --offline --all-targets`: 라이브러리 44개, CLI 3개, 통합 2개, 총 49개 통과.
- 실제 RSS 50개 수집, 재수집 시 신규 0개·갱신 50개 확인.
- 최종 규칙 적용 후 당시 최근 24시간 1개, 최근 48시간 5개 선정. 수량은 실행 시각과 신규 기사에 따라 달라집니다.
- 디스코드 실전송, 장기간 데몬 운영 및 원격 CI 실행은 수행하지 않았습니다. PowerShell 편의 스크립트는 구문 검사만 완료했으며, 실행 정책 때문에 실제 검증은 Cargo와 실행 파일로 수행했습니다.

오프라인 수집 경로도 제공됩니다. 예제 XML의 발행일은 고정되어 있으므로 현재 시각에 따라 일일 브리핑에서 제외될 수 있습니다. 통합 테스트는 별도의 기준 시각을 사용합니다.

```powershell
cargo run --locked -- --data-dir data/offline collect --fixture tests/fixtures/mixed-dates.xml
```

## 개발 시 AI 오케스트레이션

이 프로젝트는 수집기, 침해사고 판별기, 보고서·전송 모듈을 별도 에이전트에 분담하고, 다른 에이전트가 독립적으로 검토한 뒤 통합 테스트하는 방식으로 구축했습니다. 현재 실행 파일은 Rust 규칙 기반으로 동작하며 LLM API를 호출하지 않습니다. AI 토큰 사용은 이 개발·검토 세션에서 발생합니다. 운영 중 매 기사마다 다중 모델 검증을 실행하는 기능은 포함하지 않았습니다.
