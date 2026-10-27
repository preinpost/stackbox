# farm portal

기존 Rust farmd에 연결되는 웹포탈입니다.

- React + TypeScript + Vite
- Tailwind CSS 4와 Base UI: Dialog, RadioGroup, Checkbox, Select, Tabs, Button, Input
- TanStack Router: URL 기반 화면과 상세 페이지
- TanStack Query: API 캐시, polling, 작업 후 갱신, SSE 로그 캐시
- TanStack Table 8: farm 검색·상태 필터·정렬·페이지 이동

## 개발 실행

Node.js 22.12 이상이 필요합니다. 프로젝트 루트에서 farmd를 먼저 실행합니다.

```bash
cargo run -- serve
```

다른 터미널에서:

```bash
cd web
npm ci
npm run dev
# http://127.0.0.1:5180
```

기본 farmd 주소는 `http://127.0.0.1:8080`입니다. 다른 호스트나 token을 쓰는 경우
`web/.env`를 만드세요 (`.env.example` 참고).

```dotenv
FARM_API_URL=http://127.0.0.1:8080
FARM_API_TOKEN=server-token
```

`FARM_API_TOKEN`은 farmd의 token과 같아야 합니다. Vite 개발 서버와 운영 Node 서버만 읽으며
브라우저 번들에는 포함되지 않습니다. `VITE_` 접두사를 붙이지 마세요.
설정을 변경하면 포탈 서버를 재시작하세요.

## 화면

- `/`: 실제 farm 목록, 검색·상태 필터·정렬·pagination, 호스트 메모리 예약, farm 생성
- `/farms/:id`: VM 토폴로지, 메모리·디스크, 배포·재개·삭제, public endpoint 주소, 작업 내역, SSE 로그
- `/jobs`: 진행 중·완료·실패/중단된 작업과 마지막 phase
- `/host`: runtime·golden image·가상화 준비 상태, 메모리·디스크·상태 디렉터리

첫 실행 시 실제 farm이 없으면 빈 화면을 표시합니다. 예제 farm을 자동으로 생성하지 않습니다.
Farm마다 VM 한 대를 만들며, Kolla Ansible과 DB·메시지 큐·선택한 OpenStack 서비스를
모두 그 VM에 설치합니다. 기본 3 GiB에 선택 서비스의 메모리를 합산하며,
배포 가능한 서비스를 모두 선택하면 8 GiB입니다.
Keystone·Glance는 필수이며 Placement·Horizon·Neutron을 선택할 수 있습니다.
Nova는 준비 중으로 표시하고 선택할 수 없습니다. 서비스 간 통신은 같은 VM 안에서
이루어지며 단일 VM의 eth0을 사용합니다.
기존 다중 VM Farm은 기존 역할과 VM을 그대로 표시하고 삭제할 수 있습니다.
기존 Farm의 배포는 비활성화하며, 단일 VM Farm을 새로 만들도록 안내합니다.
배포는 VM 생성과 분리되며, 실패 시 마지막 배포 phase를 재개 폼의 기본값으로 제안합니다.

상태는 TanStack Query로 주기적으로 갱신하며, 로그는 `EventSource`로 수신합니다.
SSE의 `Last-Event-ID`와 initial cursor로 로그를 이어받고 중복 이벤트를 제거합니다.
최신 1,000개 이벤트를 유지하며 보관 범위를 지난 cursor는 gap 안내를 표시합니다.

## 운영 실행

```bash
cd web
npm ci
npm run build
npm start
# http://127.0.0.1:3000
```

운영 서버도 `.env`를 읽습니다. `PORT=3001 npm start`로 포트를 변경할 수 있습니다.
기본 바인딩은 loopback입니다. Node 서버는 SPA 경로를 지원하므로 상세 URL로 직접 접속하거나
새로고침할 수 있습니다. Vite preview는 API 프록시가 없으므로 운영 실행에 사용하지 않습니다.

원격 사용 시 로그인/접근 제어를 제공하는 reverse proxy 뒤에 배치하고,
프록시가 로컬 포탈로 연결하도록 구성하세요. 이 포탈 자체에는 사용자 로그인·권한 구분이 없습니다.
`PORTAL_HOST`를 변경해 직접 노출하면 해당 주소에 접근하는 사람이 VM을 관리할 수 있습니다.

## 서버 경계

```text
Browser → /api/v1/* → web/server/proxy.mjs → farmd → microsandbox VMs
```

개발·운영 모두 같은 API 프록시를 사용합니다. 서버가 bearer token을 주입하고,
교차 출처 요청과 지원하지 않는 API 경로를 차단합니다.
`/endpoints` 응답에서는 `services`만 전달하며 관리자 인증정보를 제거합니다.
화면은 비밀값이 없는 farm record의 exposure로 public URL을 표시하므로
관리자 인증정보 API를 자동으로 호출하지 않습니다. 브라우저 저장소에도 token이나 password를 쓰지 않습니다.

OpenStack 인스턴스 관리 화면과 Nova·Cinder 배포는 아직 지원하지 않습니다.

## 원격 호스트

`/hosts`에서 주소·SSH 포트·사용자와 비밀번호 또는 SSH 개인 키(선택적 passphrase)를 등록합니다.
먼저 조회한 서버 지문을 신뢰할 수 있는 서버 콘솔의 지문과 비교하고 확인해야 등록할 수 있습니다.
등록 후 SSH 점검을 시작하며, 환경 준비는 별도 버튼으로 실행합니다.

환경 준비는 Linux·동일 CPU 아키텍처·KVM 접근을 확인한 뒤 msb 0.7.8 설치, golden image 빌드/로드,
시험 VM 부팅·명령 실행·삭제를 수행합니다. Docker가 필요하면 해당 SSH 사용자로 접근 가능해야 합니다.
기존의 호환되는 runtime과 이미지는 재사용하며 다른 msb 버전을 자동으로 덮어쓰지 않습니다.

Hosts와 Jobs 화면에서 단계·실패 이유·재시작으로 중단된 작업을 확인할 수 있습니다.
인증 정보 입력은 Query/Mutation cache와 브라우저 저장소를 거치지 않고 서버로 전송하며,
등록 요청 완료/실패 후 입력을 비웁니다. 조회 API에는 인증 정보를 반환하지 않습니다.
등록 해제는 중앙 기록과 암호화된 인증 정보만 지우며 원격 설치나 VM을 삭제하지 않습니다.

Farm 생성의 `실행 호스트`에서 준비된 호스트(Hosts에서 등록한 로컬 호스트, 에이전트 호스트 또는 원격 서버)를 선택합니다.
원격 호스트는 환경 준비와 단일 VM을 지원하는 Farm 실행기 설치를 마치고 5분 이내에 SSH 점검한 상태여야 합니다.
기존 실행기가 단일 VM을 지원하지 않으면 업데이트 안내를 표시하고 신규 Farm 생성을 막습니다.
기존 실행기에 연결된 Farm의 상태 조회와 삭제는 계속 사용할 수 있습니다.
선택한 호스트의 예약 한도로 메모리를 검사하며, 공개 URL은 타겟의 IPv4/DNS로 설정하세요.
원격 서비스 포트는 타겟의 모든 IPv4 인터페이스에 바인딩합니다.
Farm 목록·상세에 실행 호스트를 표시하고, 원격 조회 실패 시 마지막 VM 상태와 연결 오류를 함께 표시합니다.
호스트에 Farm이 남아 있으면 등록 해제를 거절합니다. 인증 변경은 지문 확인 후 같은 주소·port·사용자의 인증 정보만 교체합니다.
한 Farm을 여러 호스트로 나누는 구성은 지원하지 않습니다.
조건과 보관·복구 방법은 [원격 호스트 운영](../docs/remote-hosts.md)을 참고하세요.

## 검증과 API 타입

```bash
npm test             # API 오류, SSE 재연결 병합, 프록시 token/비밀번호 보호와 streaming
npm run build        # TypeScript 검사 + production build
npm run api:types    # ../docs/openapi.json → TypeScript 타입 생성
npm run format
```

Rust API가 바뀌면 먼저 루트에서 `cargo run -- openapi > docs/openapi.json`을 실행한 후
`npm run api:types`로 타입을 갱신하세요. 화면의 API 타입은 이 스키마에서 가져옵니다.
