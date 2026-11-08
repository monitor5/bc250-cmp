# BC-250 CMP (monitor-only)

BC-250 시스템의 상태를 웹에서 확인하는 경량 모니터입니다. 각 노드에서 `bc250mon` 바이너리 하나가 수집기와 웹 UI를 함께 실행합니다. 중앙 서버, 데이터베이스, Python, Node.js, Docker는 필요하지 않습니다.

모니터링 전용으로 CPU, GPU, 메모리와 프로세스 상태를 보여줍니다. 프로세스 종료나 시스템 설정 변경 기능은 제공하지 않습니다.

## 주요 기능

- CPU 및 GPU 사용률 그래프: 15분, 1시간, 6시간, 24시간, 7일. 구간 평균과 최대값 표시
- RAM, GTT, VRAM, 스왑, GPU 및 CPU 온도, GPU 전력과 GFX 클록, 논리 코어별 CPU 사용률과 클록
- 선택 기능: [bc250-vrm-dkms](https://github.com/Hexxeh/bc250-vrm-dkms)의 hwmon 센서가 있으면 12 V 입력 및 CPU/GPU 전원 레일의 전압, 전류, 전력, 온도를 10초 간격으로 표시
- 인터페이스별 링크 속도와 실시간 RX/TX. 드라이버가 링크 속도를 제공하지 않아도 RX/TX는 표시
- 프로세스별 CPU 및 GPU 사용률, VRAM/GTT, RSS, 스레드와 명령줄. 정렬 및 필터 지원
- 최근 1시간의 2초 샘플은 메모리에, 기본 7일의 1분 평균은 디스크에 저장
- CPU 타일을 누르면 논리 코어별 사용률, GPU 타일을 누르면 GPU 상태 및 프로세스별 사용률 표시
- 노드별 SSH 접속 명령 표시 (직접 연결할 수 없는 노드는 SSH 점프 호스트 필요)
- 설정된 노드의 API를 서버가 프록시하므로 브라우저에서 직접 접근할 수 없는 노드도 표시

데이터는 Linux `/proc` 및 AMD GPU의 `/sys`와 `fdinfo`에서 읽습니다. 프로세스 목록은 화면에서 해당 목록을 볼 때 수집합니다. 네트워크와 클록은 최신 샘플만 메모리에 유지하며 별도의 기록을 저장하지 않습니다.

## 구성 예시

```text
브라우저 ──> bc250-1:9250 ──> bc250-2:9250
                 웹 UI           노드 API
```

각 노드에 같은 바이너리를 설치하고, `PEERS`에는 서비스를 실행하는 노드가 접근할 수 있는 주소를 지정합니다. 브라우저는 접속한 노드의 `/peer/<번호>/api/...` 경로를 통해 다른 노드의 데이터를 가져옵니다. 어느 노드에서든 다른 노드로 연결할 수 있다면 양쪽 데이터를 볼 수 있습니다.

## 설치

Linux x86-64용 정적 바이너리는 `dist` 브랜치에서 제공합니다. 각 노드에서 다음을 실행합니다.

```sh
curl -fL -o bc250mon-x86_64-linux.tar.gz https://github.com/monitor5/bc250-cmp/raw/refs/heads/dist/bc250mon-x86_64-linux.tar.gz
curl -fL -o SHA256SUMS https://github.com/monitor5/bc250-cmp/raw/refs/heads/dist/SHA256SUMS
sha256sum -c SHA256SUMS
tar xzf bc250mon-x86_64-linux.tar.gz
cd bc250mon
sudo ./deploy/install.sh
sudo vi /etc/default/bc250mon
sudo systemctl enable --now bc250mon
```

설정 예시 (`bc250-1` 기준):

```sh
LISTEN=192.168.0.20:9250
NAME=bc250-1
PEERS=bc250-1=http://192.168.0.20:9250,bc250-2=http://192.168.50.191:9250
DAYS=7
INTERVAL=2
```

두 번째 노드에서는 `NAME`과 `LISTEN`을 해당 노드에 맞게 바꿉니다. 양 노드에서 서로의 `PEERS` 주소로 연결할 수 있어야 합니다. 설정 변경 후 `sudo systemctl restart bc250mon`을 실행하고, 브라우저에서 첫 번째 노드의 `http://192.168.0.20:9250/`을 엽니다.

**접근 제한:** 인증과 암호화 기능이 없습니다. 신뢰할 수 있는 사설망 또는 VPN의 필요한 인터페이스에만 바인딩하고, 방화벽으로 접근을 제한하세요. `0.0.0.0`은 모든 인터페이스에 노출합니다.

GPU 상세는 AMDGPU가 인식한 활성 CU 수도 표시합니다. 현재 작업에 투입된 CU 수는 드라이버에서 제공하지 않습니다. BC-250에서는 먼저 `gpu_metrics` v2.2의 GFX 사용률을 읽습니다. [Cyan Skillfish Governor](https://github.com/filippor/cyan-skillfish-governor)의 `fix-metrics`로 제공되는 값도 지원합니다. GPU 전체 센서를 사용할 수 없으면 Linux DRM fdinfo의 GFX/compute 엔진 시간을 클라이언트별로 합산한 추정값을 사용합니다. 중복된 엔진 실행, 짧게 실행된 프로세스, KFD 직접 큐 등은 정확히 반영되지 않을 수 있으며 하드웨어 점유율이나 개별 연산 코어 사용률이 아닙니다. 카운터가 없으면 N/A로 표시합니다. 이 방식은 [nvtop](https://github.com/Syllo/nvtop)과 [BC-250 Toolkit](https://github.com/Necrosiak/bc250-toolkit-decky)의 GPU 엔진 계측 접근을 참고했습니다. SSH 버튼은 접속 명령만 표시하며 원격 셸을 웹에서 실행하지 않습니다. 예시 구성에서 두 번째 노드가 브라우저 장치에 직접 연결되지 않으면 `ssh -J bc250@192.168.0.20 bc250@192.168.50.191`을 사용하세요.

## 서비스와 자원 사용량

제공되는 systemd 서비스는 다른 사용자의 프로세스별 GPU 정보를 읽기 위해 root로 실행되며, capability를 제한합니다. `MemoryMax=16M`, `MemorySwapMax=0`, `CPUQuota=10%`가 설정되어 있습니다. 메모리 제한에 도달하면 서비스가 종료되거나 재시작될 수 있습니다. 동시 요청은 최대 8개, 프록시 응답은 최대 512 KiB로 제한합니다. 최대 크기를 넘는 기록 파일은 보존하고 디스크 기록을 비활성화합니다.

2026-11-08에 v0.2.1을 두 BC-250에서 실행하고 API와 프로세스 목록을 조회한 직후 측정한 값은 다음과 같습니다. 서비스 가동 약 4분 후의 스냅샷이며, 부하와 실행 시간에 따라 달라집니다.

| 노드 | 프로세스 RSS | systemd 서비스 메모리 현재 / 관측 최대 |
|---|---:|---:|
| bc250-1 | 820 KiB | 약 3.1 / 3.5 MiB |
| bc250-2 | 880 KiB | 약 4.6 / 4.7 MiB |

RSS는 프로세스의 상주 메모리이고 systemd 값은 서비스 cgroup의 메모리 사용량으로 파일 캐시 등을 포함할 수 있어 서로 다릅니다. 직접 확인하려면:

```sh
systemctl status bc250mon
systemctl show bc250mon -p MemoryCurrent -p MemoryPeak -p NRestarts
pid=$(systemctl show bc250mon -p MainPID --value)
grep -E '^Vm(RSS|HWM|Swap):' /proc/$pid/status
```

## 옵션과 API

```text
--listen ADDR    수신 주소 (기본 0.0.0.0:9250)
--name NAME      노드 이름 (기본 hostname)
--peers LIST     name=http://ip:port,name2=http://ip2:port
--data FILE      기록 파일 (기본 /var/lib/bc250mon/history.bin, none이면 RAM만 사용)
--days N         1분 기록 보관 일수 (기본 7, 최대 30)
--interval SEC   샘플 간격 (기본 2초)
--gpu-dev PATH   amdgpu sysfs 경로 (기본 자동 탐지)
```

- `GET /api/now`: 현재 값
- `GET /api/hist?sec=3600&n=600`: 기간별 기록
- `GET /api/procs?n=300`: 프로세스 목록
- `GET /api/info`: 노드와 peer 설정

## 소스에서 빌드

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cargo test --release
```
