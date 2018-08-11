'use strict';

let content = document.getElementById('content');
let submit = document.getElementById('submit-button');
let submitcancel = document.getElementById('cancel-button');
let log = document.getElementById('log');
let statusdiv = document.getElementById('status-div');
let link = document.getElementById('link');
let uploadmeter = document.getElementById('uploadmeter');

let reportStatus = function (newStatus) {
  log.value += newStatus + '\n';
  log.scrollTop = log.scrollHeight;
};

submit.disabled = false;
log.value = '';

function addEventHandlers(xhr, loadedCallback, errorCallback) {
  xhr.onload = function () {
    if (xhr.status != 200) {
      errorCallback('http-error', xhr);
    } else {
      loadedCallback(xhr);
    }
  };
  xhr.onerror = function () {
    errorCallback('error', xhr);
  };
  xhr.onabort = function () {
    errorCallback('abort', xhr);
  };
  xhr.ontimeout = function () {
    errorCallback('timeout', xhr);
  }
}

// https://stackoverflow.com/a/23329386/3444805
function utf8ByteLength(str) {
  // returns the byte length of an utf8 string
  var s = str.length;
  for (var i=str.length-1; i>=0; i--) {
    var code = str.charCodeAt(i);
    if (code > 0x7f && code <= 0x7ff) s++;
    else if (code > 0x7ff && code <= 0xffff) s+=2;
    if (code >= 0xDC00 && code <= 0xDFFF) i--; //trail surrogate
  }
  return s;
}

function requestId(value, loadedCallback, errorCallback) {
  reportStatus('Requesting upload id');

  let xhr = new XMLHttpRequest();
  xhr.open('POST', '/1/id/request?length=' + utf8ByteLength(value), true);
  addEventHandlers(xhr, loadedCallback, errorCallback);
  xhr.send();

  return xhr;
}

function cancelId(id, secret, loadedCallback, errorCallback) {
  reportStatus('Cancelling');

  let xhr = new XMLHttpRequest();
  xhr.open('POST', '/1/id/retire?id=' + id + '&secret=' + secret);
  xhr.send();
  addEventHandlers(xhr, loadedCallback, errorCallback);

  return xhr;
}

function upload(value, id, secret, loadedCallback, errorCallback) {
  reportStatus('Starting upload');

  let xhr = new XMLHttpRequest();
  xhr.open('POST', '/1/file/upload?id=' + id + '&secret=' + secret);
  xhr.setRequestHeader("Content-Type", "text/plain; charset=utf-8");
  addEventHandlers(xhr, loadedCallback, errorCallback);
  xhr.send(value);

  return xhr;
}

function reset() {
  statusdiv.hidden = true;

  submit.disabled = false;
  submit.innerText = 'Upload';
  submitcancel.hidden = true;
  delete submitcancel.onclick;
}

submit.onclick = function onclickSubmit () {
  let id = '';
  let secret = '';
  let uploads = 0;
  let errors = 0;
  let value = content.value;
  let curxhr = null;
  link.value = '';
  uploadmeter.innerText = '0';

  statusdiv.hidden = false;
  log.value = '';

  submit.disabled = true;
  submit.innerText = 'Uploading...';
  submitcancel.hidden = false;
  submitcancel.onclick = function () {
    curxhr.abort();

    cancelId(id, secret, function () {
      reportStatus('Id retired.');
      reset();
    }, function (type, xhr) {
      if (type === 'http-error') {
        reportStatus('Cancel failed with HTTP error ' + xhr.status + ', ' + xhr.responseText);
      } else {
        reportStatus('Cancel failed.');
      }
    });
  };

  function uploadSuccess (xhr) {
    reportStatus('Upload ok: "' + xhr.responseText + '"')
    uploads += 1;
    uploadmeter.innerText = '' + uploads;
    errors = 0;

    // get it ready to go again
    curxhr = upload(value, id, secret, uploadSuccess, uploadError);
  }

  function uploadError (type, xhr) {
    if (type === 'http-error') {
      reportStatus('Upload failed with HTTP error ' + xhr.status + ', ' + xhr.responseText);
    } else if (type === 'abort') {
      reportStatus('Upload aborted.');
      return;
    } else if (type === 'timeout') {
      reportStatus('Upload timeout.');
    } else {
      reportStatus('Upload failed.');
    }

    reportStatus('Upload failed: "' + xhr.responseText + '"')

    errors += 1;

    if (errors < 20) {
      // try to start a new upload anyway
      // TODO check what the error actually was
      curxhr = upload(value, id, secret, uploadSuccess, uploadError);
    } else {
      reportStatus('Too many errors, giving up');
      reset();
    }
  }

  requestId(
    value,
    function requestIdLoaded (xhr) {
      let parts = xhr.responseText.split(',');
      id = parts[0];
      secret = parts[1];
      uploads = 0;
      uploadmeter.innerText = '0';

      let url = window.location;
      link.value = url.protocol + '//' + url.host + '/1/file/download?id=' + id;
      link.size = '' + (link.value.length);

      reportStatus('Got id');
      curxhr = upload(value, id, secret, uploadSuccess, uploadError);
    },
    function requestIdError (type, xhr) {
      // TODO retry?
      reportStatus('Request failed.');
    }
  );
};
