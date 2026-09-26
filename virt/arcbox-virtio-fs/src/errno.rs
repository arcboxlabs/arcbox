//! Errno numbering for FUSE replies.

/// Translates a host errno into the Linux errno the guest kernel expects.
///
/// Pass host values (`libc::E*`, `io::Error::raw_os_error`). A Darwin errno
/// with no Linux counterpart becomes `EIO`; `0` stays `0`.
#[cfg(target_os = "macos")]
#[must_use]
pub const fn linux_errno(host_errno: i32) -> i32 {
    use linux_raw_sys::errno as linux;

    let errno = match host_errno {
        // Darwin numbers 1-34 like Linux, except EDEADLK.
        0..=10 | 12..=34 => return host_errno,
        libc::EDEADLK => linux::EDEADLK,
        libc::EAGAIN => linux::EAGAIN,
        libc::ENAMETOOLONG => linux::ENAMETOOLONG,
        libc::ENOLCK => linux::ENOLCK,
        libc::ENOSYS => linux::ENOSYS,
        libc::ENOTEMPTY => linux::ENOTEMPTY,
        libc::ELOOP => linux::ELOOP,
        libc::ENOMSG => linux::ENOMSG,
        libc::EIDRM => linux::EIDRM,
        libc::ENOSTR => linux::ENOSTR,
        // Linux has no ENOATTR; its xattr calls report ENODATA.
        libc::ENOATTR | libc::ENODATA => linux::ENODATA,
        libc::ETIME => linux::ETIME,
        libc::ENOSR => linux::ENOSR,
        libc::EREMOTE => linux::EREMOTE,
        libc::ENOLINK => linux::ENOLINK,
        libc::EPROTO => linux::EPROTO,
        libc::EMULTIHOP => linux::EMULTIHOP,
        libc::EBADMSG => linux::EBADMSG,
        libc::EOVERFLOW => linux::EOVERFLOW,
        libc::EILSEQ => linux::EILSEQ,
        libc::EUSERS => linux::EUSERS,
        libc::ENOTSOCK => linux::ENOTSOCK,
        libc::EDESTADDRREQ => linux::EDESTADDRREQ,
        libc::EMSGSIZE => linux::EMSGSIZE,
        libc::EPROTOTYPE => linux::EPROTOTYPE,
        libc::ENOPROTOOPT => linux::ENOPROTOOPT,
        libc::EPROTONOSUPPORT => linux::EPROTONOSUPPORT,
        libc::ESOCKTNOSUPPORT => linux::ESOCKTNOSUPPORT,
        // Linux defines ENOTSUP as EOPNOTSUPP; Darwin keeps them apart.
        libc::ENOTSUP | libc::EOPNOTSUPP => linux::EOPNOTSUPP,
        libc::EPFNOSUPPORT => linux::EPFNOSUPPORT,
        libc::EAFNOSUPPORT => linux::EAFNOSUPPORT,
        libc::EADDRINUSE => linux::EADDRINUSE,
        libc::EADDRNOTAVAIL => linux::EADDRNOTAVAIL,
        libc::ENETDOWN => linux::ENETDOWN,
        libc::ENETUNREACH => linux::ENETUNREACH,
        libc::ENETRESET => linux::ENETRESET,
        libc::ECONNABORTED => linux::ECONNABORTED,
        libc::ECONNRESET => linux::ECONNRESET,
        libc::ENOBUFS => linux::ENOBUFS,
        libc::EISCONN => linux::EISCONN,
        libc::ENOTCONN => linux::ENOTCONN,
        libc::ESHUTDOWN => linux::ESHUTDOWN,
        libc::ETOOMANYREFS => linux::ETOOMANYREFS,
        libc::ETIMEDOUT => linux::ETIMEDOUT,
        libc::ECONNREFUSED => linux::ECONNREFUSED,
        libc::EHOSTDOWN => linux::EHOSTDOWN,
        libc::EHOSTUNREACH => linux::EHOSTUNREACH,
        libc::EALREADY => linux::EALREADY,
        libc::EINPROGRESS => linux::EINPROGRESS,
        libc::ESTALE => linux::ESTALE,
        libc::EDQUOT => linux::EDQUOT,
        libc::ECANCELED => linux::ECANCELED,
        libc::EOWNERDEAD => linux::EOWNERDEAD,
        libc::ENOTRECOVERABLE => linux::ENOTRECOVERABLE,
        _ => linux::EIO,
    };
    errno.cast_signed()
}

/// Translates a host errno into the Linux errno the guest kernel expects.
///
/// A Linux host already uses Linux numbering, so the value passes through.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub const fn linux_errno(host_errno: i32) -> i32 {
    host_errno
}

#[cfg(test)]
mod tests {
    use super::linux_errno;

    #[test]
    fn shared_numbers_pass_through() {
        for errno in [
            0,
            libc::EPERM,
            libc::ENOENT,
            libc::EIO,
            libc::EBADF,
            libc::EACCES,
            libc::EEXIST,
            libc::EINVAL,
            libc::ERANGE,
        ] {
            assert_eq!(linux_errno(errno), errno);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_numbers_become_linux_numbers() {
        assert_eq!(linux_errno(libc::EAGAIN), 11);
        assert_eq!(linux_errno(libc::EDEADLK), 35);
        assert_eq!(linux_errno(libc::ENAMETOOLONG), 36);
        assert_eq!(linux_errno(libc::ENOSYS), 38);
        assert_eq!(linux_errno(libc::ENOTEMPTY), 39);
        assert_eq!(linux_errno(libc::ELOOP), 40);
        assert_eq!(linux_errno(libc::ENOATTR), 61);
        assert_eq!(linux_errno(libc::ENOTSUP), 95);
        assert_eq!(linux_errno(libc::EOPNOTSUPP), 95);
        assert_eq!(linux_errno(libc::ESTALE), 116);
        assert_eq!(linux_errno(libc::EDQUOT), 122);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_only_numbers_become_eio() {
        assert_eq!(linux_errno(libc::ENOPOLICY), libc::EIO);
        assert_eq!(linux_errno(libc::EQFULL), libc::EIO);
    }
}
