// Package atomicfile 은 같은 폴더 임시 파일에 다 쓰고 디스크에 내린 뒤 바꿔 끼우는 쓰기다(R5.2).
// 쓰기·바꿔 끼우기가 실패하면 원래 파일이 그대로 남고 임시 파일은 지운다.
//
// 한계: 바꿔 끼운 뒤 폴더는 fsync 하지 않아 전원이 나가면 이름 바뀜이 사라질 수 있다(내용은 옛 판이나 새 판 중 하나).
// 새 파일은 perm 만 맞추며 원래 파일의 소유자·그룹·setuid/setgid·확장 속성은 지키지 않는다.
package atomicfile

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"syscall"
)

// Write 는 data 를 path 에 바꿔 끼워 쓴다. pattern 은 os.CreateTemp 의 임시 파일 이름 꼴, perm 은 새 파일 권한이다.
func Write(path, pattern string, data []byte, perm os.FileMode) error {
	tmp, err := os.CreateTemp(filepath.Dir(path), pattern) // 0600 으로 만든다
	if err != nil {
		return err
	}
	_, werr := tmp.Write(data)
	if err := errors.Join(werr, tmp.Sync(), tmp.Close(), os.Chmod(tmp.Name(), perm)); err != nil {
		return errors.Join(err, os.Remove(tmp.Name()))
	}
	if err := os.Rename(tmp.Name(), path); err != nil {
		return errors.Join(err, os.Remove(tmp.Name()))
	}
	return nil
}

// Lock 은 path 옆 .lock 파일에 배타 flock 을 건다. 돌려준 파일을 닫으면 풀린다. 다시 읽고 합쳐 바꿔 끼우는 쓰기를
// 프로세스끼리 줄 세운다. 바꿔 끼우는 파일 자체에 걸면 잠금이 옛 inode 에 남으므로 따로 둔다. 배포 대상(linux·darwin)은 flock 이 있다.
func Lock(path string) (*os.File, error) {
	f, err := os.OpenFile(path+".lock", os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX); err != nil {
		return nil, errors.Join(fmt.Errorf("lock %s: %w", filepath.Base(path), err), f.Close())
	}
	return f, nil
}
