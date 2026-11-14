// Package atomicfile 은 같은 폴더 임시 파일에 다 쓰고 디스크에 내린 뒤 바꿔 끼우는 쓰기다(R5.2).
// 쓰기·바꿔 끼우기가 실패하면 원래 파일이 그대로 남고 임시 파일은 지운다.
package atomicfile

import (
	"errors"
	"os"
	"path/filepath"
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
