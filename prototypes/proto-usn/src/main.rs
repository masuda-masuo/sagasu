//! proto-usn: M2 本命経路の検証(Windows 専用)。
//! NTFS USN Journal に「マーカー以降の変更ファイル一覧」を問い合わせ、
//! その速度(= 検索時差分マージの差分照会コスト)を計測する。
//!
//! 使い方(管理者権限のターミナルで):
//!   proto-usn C:                         # ジャーナルの状態を表示(NextUsn がマーカー)
//!   ... ファイルをいくつか作成・変更・削除 ...
//!   proto-usn C: --since <NextUsn>       # マーカー以降の変更レコードを列挙 + 所要時間を表示
//!
//! 注意: FSCTL_READ_USN_JOURNAL はボリュームハンドルへの読み取りに管理者権限が要る。
//! ファイル名しか取れない(フルパスは FRN→親 FRN の解決が別途必要 = 次の検証項目)。

#[cfg(not(windows))]
fn main() {
    eprintln!("proto-usn は Windows 専用です(NTFS USN Journal の検証)。");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::run()
}

#[cfg(windows)]
mod win {
    use std::time::Instant;

    use anyhow::{Context, Result};
    use clap::Parser;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0,
        USN_JOURNAL_DATA_V0, USN_RECORD_V2,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    #[derive(Parser)]
    #[command(about = "NTFS USN Journal の差分照会プロトタイプ(要管理者権限)")]
    struct Args {
        /// 対象ボリューム(例: C:)
        #[arg(default_value = "C:")]
        volume: String,
        /// この USN 以降の変更レコードを読む(省略時はジャーナル状態のみ表示)
        #[arg(long)]
        since: Option<i64>,
        /// 表示する最大レコード数
        #[arg(long, default_value_t = 2000)]
        max_records: usize,
        /// CLOSE レコード(reason に USN_REASON_CLOSE を含む)のみ表示する
        #[arg(long)]
        close_only: bool,
    }

    const REASONS: &[(u32, &str)] = &[
        (0x0000_0001, "DATA_OVERWRITE"),
        (0x0000_0002, "DATA_EXTEND"),
        (0x0000_0004, "DATA_TRUNCATION"),
        (0x0000_0100, "FILE_CREATE"),
        (0x0000_0200, "FILE_DELETE"),
        (0x0000_1000, "RENAME_OLD_NAME"),
        (0x0000_2000, "RENAME_NEW_NAME"),
        (0x0000_8000, "BASIC_INFO_CHANGE"),
        (0x8000_0000, "CLOSE"),
    ];

    fn reason_str(mask: u32) -> String {
        let names: Vec<&str> =
            REASONS.iter().filter(|(bit, _)| mask & bit != 0).map(|&(_, n)| n).collect();
        if names.is_empty() {
            format!("{mask:#010x}")
        } else {
            names.join("|")
        }
    }

    fn open_volume(volume: &str) -> Result<HANDLE> {
        let vol = volume.trim_end_matches(['\\', '/']);
        let path = format!(r"\\.\{vol}");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .with_context(|| format!("ボリューム {path} を開けない(管理者権限で実行しているか?)"))
    }

    fn query_journal(handle: HANDLE) -> Result<USN_JOURNAL_DATA_V0> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                handle,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut data as *mut _ as *mut _),
                std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut returned),
                None,
            )
        }
        .context("FSCTL_QUERY_USN_JOURNAL 失敗(ジャーナル無効のボリューム?)")?;
        Ok(data)
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();
        let handle = open_volume(&args.volume)?;
        let result = run_inner(&args, handle);
        unsafe {
            let _ = CloseHandle(handle);
        }
        result
    }

    fn run_inner(args: &Args, handle: HANDLE) -> Result<()> {
        let journal = query_journal(handle)?;
        println!("volume        : {}", args.volume);
        println!("journal id    : {:#x}", journal.UsnJournalID);
        println!("first usn     : {}", journal.FirstUsn);
        println!("next usn      : {}  <- 索引時点マーカーとして保存する値", journal.NextUsn);

        let Some(since) = args.since else {
            println!();
            println!("--since <NextUsn> を付けて再実行すると、その時点以降の変更を列挙します。");
            return Ok(());
        };

        let t0 = Instant::now();
        // 8 バイトアライン済みバッファ(先頭 8 バイトは次回開始 USN)
        let mut buf = vec![0u64; 64 * 1024 / 8];
        let mut start_usn = since;
        let mut shown = 0usize;
        let mut total = 0usize;

        'outer: loop {
            let read_data = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: start_usn,
                ReasonMask: 0xFFFF_FFFF,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: journal.UsnJournalID,
            };
            let mut returned = 0u32;
            unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_READ_USN_JOURNAL,
                    Some(&read_data as *const _ as *const _),
                    std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                    Some(buf.as_mut_ptr() as *mut _),
                    (buf.len() * 8) as u32,
                    Some(&mut returned),
                    None,
                )
            }
            .context("FSCTL_READ_USN_JOURNAL 失敗")?;

            if returned <= 8 {
                break; // レコードなし
            }
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, returned as usize) };
            start_usn = i64::from_le_bytes(bytes[0..8].try_into().unwrap());

            let mut offset = 8usize;
            while offset + std::mem::size_of::<USN_RECORD_V2>() <= returned as usize {
                // buf は 8 バイトアライン、レコード境界も 8 バイトアラインなので参照は安全
                let rec = unsafe { &*(bytes.as_ptr().add(offset) as *const USN_RECORD_V2) };
                if rec.RecordLength == 0 {
                    break;
                }
                total += 1;
                let is_close = rec.Reason & 0x8000_0000 != 0;
                if (!args.close_only || is_close) && shown < args.max_records {
                    let name_off = offset + rec.FileNameOffset as usize;
                    let name_len = rec.FileNameLength as usize / 2;
                    let name_u16 = unsafe {
                        std::slice::from_raw_parts(bytes.as_ptr().add(name_off) as *const u16, name_len)
                    };
                    let name = String::from_utf16_lossy(name_u16);
                    println!(
                        "  usn={:<14} frn={:#018x} {:<40} {}",
                        rec.Usn,
                        rec.FileReferenceNumber,
                        reason_str(rec.Reason),
                        name
                    );
                    shown += 1;
                }
                if shown >= args.max_records && !args.close_only {
                    break 'outer;
                }
                offset += rec.RecordLength as usize;
            }
        }

        let elapsed = t0.elapsed();
        println!();
        println!("records       : {total}(表示 {shown})");
        println!("elapsed       : {:.1}ms  <- 検索時差分照会のコストに相当", elapsed.as_secs_f64() * 1000.0);
        println!("next marker   : {start_usn}");
        Ok(())
    }
}
