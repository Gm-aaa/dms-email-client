//! 平台相关的进程内存调优。均为 glibc 专有，其他 libc 目标为 no-op。
//! 常驻守护进程在同步尖峰（下载/解析一批邮件头）后容易出现 RSS 迟迟不回落，这两个
//! 钩子用来抑制/收敛这种预留。

/// glibc 内存调优：把 malloc arena 上限设为 2，抑制多线程下 RSS 的过度预留。
/// 在守护进程启动时调用一次。
pub fn tune_allocator() {
    #[cfg(target_env = "gnu")]
    {
        // M_ARENA_MAX = -8（见 glibc <malloc.h>）
        extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        unsafe {
            mallopt(-8, 2);
        }
    }
}

/// 把空闲堆页归还给 OS，用于同步尖峰（下载/解析一批头部、加载/卸载模型）之后收敛 RSS。
pub fn release_free_memory() {
    #[cfg(target_env = "gnu")]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        unsafe {
            malloc_trim(0);
        }
    }
}
