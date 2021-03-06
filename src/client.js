'use strict';

var content = document.getElementById('content');
var submit = document.getElementById('submit-button');
var submitcancel = document.getElementById('cancel-button');
var log = document.getElementById('log');
var statusdiv = document.getElementById('status-div');
var link = document.getElementById('link');
var uploadmeter = document.getElementById('uploadmeter');

var reportStatus = function (newStatus) {
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

  var xhr = new XMLHttpRequest();
  xhr.open('POST', '/1/id/request?length=' + utf8ByteLength(value), true);
  addEventHandlers(xhr, loadedCallback, errorCallback);
  xhr.send();

  return xhr;
}

function cancelId(id, secret, loadedCallback, errorCallback) {
  reportStatus('Cancelling');

  var xhr = new XMLHttpRequest();
  xhr.open('POST', '/1/id/retire?id=' + id + '&secret=' + secret);
  xhr.send();
  addEventHandlers(xhr, loadedCallback, errorCallback);

  return xhr;
}

function upload(value, id, secret, loadedCallback, errorCallback) {
  reportStatus('Starting upload');

  var xhr = new XMLHttpRequest();
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
  window.removeEventListener('beforeunload', unloadWarning);
}

function unloadWarning (e) {
  e.returnValue = "If you leave this page your paste will be unavailable!";
}

submit.onclick = function onclickSubmit () {
  var id = '';
  var secret = '';
  var uploads = 0;
  var errors = 0;
  var value = content.value;
  var curxhr = null;
  link.value = '';
  uploadmeter.innerText = '0';

  statusdiv.hidden = false;
  log.value = '';

  submit.disabled = true;
  submit.innerText = 'Uploading...';
  submitcancel.hidden = false;
  submitcancel.onclick = function () {
    curxhr.abort();
    curxhr = null;

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

  function cancelWhenUnloaded () {
    if (curxhr) {
      // Here we attempt to automatically retire the transfer when navigating away from the page.
      // We don't depend on this, but it will allow the server to pick up the change before its
      // periodic timeout.
      curxhr.abort();
      curxhr = null;

      if (navigator && navigator.sendBeacon) {
        navigator.sendBeacon('/1/id/retire?id=' + id + '&secret=' + secret);
      } else {
        var xhr = new XMLHttpRequest();
        xhr.open('POST', '/1/id/retire?id=' + id + '&secret=' + secret, false);
        xhr.send();
      }
    }
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
      var url = window.location;
      var parts = xhr.responseText.split(',');
      id = parts[0];
      secret = parts[1];
      uploads = 0;
      uploadmeter.innerText = '0';

      link.value = url.protocol + '//' + url.host + '/1/file/download?id=' + id;
      link.size = '' + (link.value.length);

      window.addEventListener('beforeunload', unloadWarning);
      window.addEventListener('unload', cancelWhenUnloaded);

      reportStatus('Got id');
      curxhr = upload(value, id, secret, uploadSuccess, uploadError);
    },
    function requestIdError (type, xhr) {
      // TODO retry?
      reportStatus('Request failed.');
    }
  );
};
